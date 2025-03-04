// Copyright 2023-2024 The NativeLink Authors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::marker::Send;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::BytesMut;
use futures::stream::{unfold, FuturesUnordered};
use futures::{future, Future, Stream, StreamExt, TryStreamExt};
use nativelink_error::{error_if, make_input_err, Error, ResultExt};
use nativelink_proto::build::bazel::remote::execution::v2::action_cache_client::ActionCacheClient;
use nativelink_proto::build::bazel::remote::execution::v2::content_addressable_storage_client::ContentAddressableStorageClient;
use nativelink_proto::build::bazel::remote::execution::v2::{
    digest_function, ActionResult, BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, FindMissingBlobsRequest, FindMissingBlobsResponse, GetActionResultRequest,
    GetTreeRequest, GetTreeResponse, UpdateActionResultRequest,
};
use nativelink_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use nativelink_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest, WriteResponse,
};
use nativelink_util::buf_channel::{DropCloserReadHalf, DropCloserWriteHalf};
use nativelink_util::common::DigestInfo;
use nativelink_util::grpc_utils::ConnectionManager;
use nativelink_util::health_utils::HealthStatusIndicator;
use nativelink_util::resource_info::ResourceInfo;
use nativelink_util::retry::{Retrier, RetryResult};
use nativelink_util::store_trait::{Store, UploadSizeInfo};
use nativelink_util::write_request_stream_wrapper::WriteRequestStreamWrapper;
use nativelink_util::{default_health_status_indicator, tls_utils};
use parking_lot::Mutex;
use prost::Message;
use rand::rngs::OsRng;
use rand::Rng;
use tokio::time::sleep;
use tonic::{IntoRequest, Request, Response, Status, Streaming};
use tracing::error;
use uuid::Uuid;

use crate::ac_utils::ESTIMATED_DIGEST_SIZE;

// This store is usually a pass-through store, but can also be used as a CAS store. Using it as an
// AC store has one major side-effect... The has() function may not give the proper size of the
// underlying data. This might cause issues if embedded in certain stores.
pub struct GrpcStore {
    instance_name: String,
    store_type: nativelink_config::stores::StoreType,
    retrier: Retrier,
    connection_manager: ConnectionManager,
}

/// This provides a buffer for the first response from GrpcStore.read in order
/// to allow the first read to occur within the retry loop.  That means that if
/// the connection establishes fine, but reading the first byte of the file
/// fails we have the ability to retry before returning to the caller.
struct FirstStream {
    /// Contains the first response from the stream (which could be an EOF,
    /// hence the nested Option).  This should be populated on creation and
    /// returned as the first result from the stream.  Subsequent reads from the
    /// stream will use the encapsulated stream.
    first_response: Option<Option<ReadResponse>>,
    /// The stream to get responses from when first_response is None.
    stream: Streaming<ReadResponse>,
}

impl Stream for FirstStream {
    type Item = Result<ReadResponse, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        if let Some(first_response) = self.first_response.take() {
            return std::task::Poll::Ready(first_response.map(Ok));
        }
        Pin::new(&mut self.stream).poll_next(cx)
    }
}

/// This structure wraps all of the information required to perform a write
/// request on the GrpcStore.  It stores the last message retrieved which allows
/// the write to resume since the UUID allows upload resume at the server.
struct WriteState<T, E>
where
    T: Stream<Item = Result<WriteRequest, E>> + Unpin + Send + 'static,
    E: Into<Error> + 'static,
{
    instance_name: String,
    read_stream_error: Option<Error>,
    read_stream: WriteRequestStreamWrapper<T, E>,
    // Tonic doesn't appear to report an error until it has taken two messages,
    // therefore we are required to buffer the last two messages.
    cached_messages: [Option<WriteRequest>; 2],
    // When resuming after an error, the previous messages are cloned into this
    // queue upfront to allow them to be served back.
    resume_queue: [Option<WriteRequest>; 2],
    // An optimisation to avoid having to manage resume_queue when it's empty.
    is_resumed: bool,
}

impl<T, E> WriteState<T, E>
where
    T: Stream<Item = Result<WriteRequest, E>> + Unpin + Send + 'static,
    E: Into<Error> + 'static,
{
    fn new(instance_name: String, read_stream: WriteRequestStreamWrapper<T, E>) -> Self {
        Self {
            instance_name,
            read_stream_error: None,
            read_stream,
            cached_messages: [None, None],
            resume_queue: [None, None],
            is_resumed: false,
        }
    }

    fn push_message(&mut self, message: WriteRequest) {
        self.cached_messages.swap(0, 1);
        self.cached_messages[0] = Some(message);
    }

    fn resumed_message(&mut self) -> Option<WriteRequest> {
        if self.is_resumed {
            // The resume_queue is a circular buffer, that we have to shift,
            // since its only got two elements its a trivial swap.
            self.resume_queue.swap(0, 1);
            let message = self.resume_queue[0].take();
            if message.is_none() {
                self.is_resumed = false;
            }
            message
        } else {
            None
        }
    }

    fn can_resume(&self) -> bool {
        self.read_stream_error.is_none() && (self.cached_messages[0].is_some() || self.read_stream.is_first_msg())
    }

    fn resume(&mut self) {
        self.resume_queue = self.cached_messages.clone();
        self.is_resumed = true;
    }
}

/// A wrapper around WriteState to allow it to be reclaimed from the underlying
/// write call in the case of failure.
struct WriteStateWrapper<T, E>
where
    T: Stream<Item = Result<WriteRequest, E>> + Unpin + Send + 'static,
    E: Into<Error> + 'static,
{
    shared_state: Arc<Mutex<WriteState<T, E>>>,
}

impl<T, E> Stream for WriteStateWrapper<T, E>
where
    T: Stream<Item = Result<WriteRequest, E>> + Unpin + Send + 'static,
    E: Into<Error> + 'static,
{
    type Item = WriteRequest;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // This should be an uncontended lock since write was called.
        let mut local_state = self.shared_state.lock();
        // If this is the first or second call after a failure and we have
        // cached messages, then use the cached write requests.
        let cached_message = local_state.resumed_message();
        if cached_message.is_some() {
            return Poll::Ready(cached_message);
        }
        // Read a new write request from the downstream.
        let Poll::Ready(maybe_message) = Pin::new(&mut local_state.read_stream).poll_next(cx) else {
            return Poll::Pending;
        };
        // Update the instance name in the write request and forward it on.
        const IS_UPLOAD_TRUE: bool = true;
        let result = match maybe_message {
            Some(Ok(mut message)) => match ResourceInfo::new(&message.resource_name, IS_UPLOAD_TRUE) {
                Ok(mut resource_name) => {
                    if resource_name.instance_name != local_state.instance_name {
                        resource_name.instance_name = &local_state.instance_name;
                        message.resource_name = resource_name.to_string(IS_UPLOAD_TRUE);
                    }
                    // Cache the last request in case there is an error to allow
                    // the upload to be resumed.
                    local_state.push_message(message.clone());
                    Some(message)
                }
                Err(err) => {
                    local_state.read_stream_error = Some(err);
                    None
                }
            },
            Some(Err(err)) => {
                local_state.read_stream_error = Some(err);
                None
            }
            None => None,
        };
        Poll::Ready(result)
    }
}

impl GrpcStore {
    pub async fn new(config: &nativelink_config::stores::GrpcStore) -> Result<Self, Error> {
        let jitter_amt = config.retry.jitter;
        Self::new_with_jitter(
            config,
            Box::new(move |delay: Duration| {
                if jitter_amt == 0. {
                    return delay;
                }
                let min = 1. - (jitter_amt / 2.);
                let max = 1. + (jitter_amt / 2.);
                delay.mul_f32(OsRng.gen_range(min..max))
            }),
        )
        .await
    }

    pub async fn new_with_jitter(
        config: &nativelink_config::stores::GrpcStore,
        jitter_fn: Box<dyn Fn(Duration) -> Duration + Send + Sync>,
    ) -> Result<Self, Error> {
        error_if!(config.endpoints.is_empty(), "Expected at least 1 endpoint in GrpcStore");
        let mut endpoints = Vec::with_capacity(config.endpoints.len());
        for endpoint_config in &config.endpoints {
            let endpoint = tls_utils::endpoint(endpoint_config)
                .map_err(|e| make_input_err!("Invalid URI for GrpcStore endpoint : {e:?}"))?;
            endpoints.push(endpoint);
        }

        Ok(GrpcStore {
            instance_name: config.instance_name.clone(),
            store_type: config.store_type,
            retrier: Retrier::new(
                Arc::new(|duration| Box::pin(sleep(duration))),
                Arc::new(jitter_fn),
                config.retry.to_owned(),
            ),
            connection_manager: ConnectionManager::new(endpoints.into_iter(), config.max_concurrent_requests),
        })
    }

    async fn perform_request<F, Fut, R, I>(&self, input: I, mut request: F) -> Result<R, Error>
    where
        F: FnMut(I) -> Fut + Send + Copy,
        Fut: Future<Output = Result<R, Error>> + Send,
        R: Send,
        I: Send + Clone,
    {
        self.retrier
            .retry(unfold(input, move |input| async move {
                let input_clone = input.clone();
                Some((
                    request(input_clone)
                        .await
                        .map_or_else(RetryResult::Retry, RetryResult::Ok),
                    input,
                ))
            }))
            .await
    }

    pub async fn find_missing_blobs(
        &self,
        grpc_request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Error> {
        error_if!(
            matches!(self.store_type, nativelink_config::stores::StoreType::ac),
            "CAS operation on AC store"
        );

        let mut request = grpc_request.into_inner();
        request.instance_name = self.instance_name.clone();
        self.perform_request(request, |request| async move {
            let (connection, channel) = self.connection_manager.get_connection().await;
            let result = ContentAddressableStorageClient::new(channel)
                .find_missing_blobs(Request::new(request))
                .await
                .err_tip(|| "in GrpcStore::find_missing_blobs");
            if let Err(err) = &result {
                connection.on_error(err);
            }
            result
        })
        .await
    }

    pub async fn batch_update_blobs(
        &self,
        grpc_request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Error> {
        error_if!(
            matches!(self.store_type, nativelink_config::stores::StoreType::ac),
            "CAS operation on AC store"
        );

        let mut request = grpc_request.into_inner();
        request.instance_name = self.instance_name.clone();
        self.perform_request(request, |request| async move {
            let (connection, channel) = self.connection_manager.get_connection().await;
            let result = ContentAddressableStorageClient::new(channel)
                .batch_update_blobs(Request::new(request))
                .await
                .err_tip(|| "in GrpcStore::batch_update_blobs");
            if let Err(err) = &result {
                connection.on_error(err);
            }
            result
        })
        .await
    }

    pub async fn batch_read_blobs(
        &self,
        grpc_request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Error> {
        error_if!(
            matches!(self.store_type, nativelink_config::stores::StoreType::ac),
            "CAS operation on AC store"
        );

        let mut request = grpc_request.into_inner();
        request.instance_name = self.instance_name.clone();
        self.perform_request(request, |request| async move {
            let (connection, channel) = self.connection_manager.get_connection().await;
            let result = ContentAddressableStorageClient::new(channel)
                .batch_read_blobs(Request::new(request))
                .await
                .err_tip(|| "in GrpcStore::batch_read_blobs");
            if let Err(err) = &result {
                connection.on_error(err);
            }
            result
        })
        .await
    }

    pub async fn get_tree(
        &self,
        grpc_request: Request<GetTreeRequest>,
    ) -> Result<Response<Streaming<GetTreeResponse>>, Error> {
        error_if!(
            matches!(self.store_type, nativelink_config::stores::StoreType::ac),
            "CAS operation on AC store"
        );

        let mut request = grpc_request.into_inner();
        request.instance_name = self.instance_name.clone();
        self.perform_request(request, |request| async move {
            let (connection, channel) = self.connection_manager.get_connection().await;
            let result = ContentAddressableStorageClient::new(channel)
                .get_tree(Request::new(request))
                .await
                .err_tip(|| "in GrpcStore::get_tree");
            if let Err(err) = &result {
                connection.on_error(err);
            }
            result
        })
        .await
    }

    fn get_read_request(&self, mut request: ReadRequest) -> Result<ReadRequest, Error> {
        const IS_UPLOAD_FALSE: bool = false;
        let mut resource_info = ResourceInfo::new(&request.resource_name, IS_UPLOAD_FALSE)?;
        if resource_info.instance_name != self.instance_name {
            resource_info.instance_name = &self.instance_name;
            request.resource_name = resource_info.to_string(IS_UPLOAD_FALSE);
        }
        Ok(request)
    }

    async fn read_internal(
        &self,
        request: ReadRequest,
    ) -> Result<impl Stream<Item = Result<ReadResponse, Status>>, Error> {
        let (connection, channel) = self.connection_manager.get_connection().await;
        let result = ByteStreamClient::new(channel)
            .read(Request::new(request))
            .await
            .err_tip(|| "in GrpcStore::read");
        if let Err(err) = &result {
            connection.on_error(err);
        }
        let mut response = result?.into_inner();
        let first_response = response
            .message()
            .await
            .err_tip(|| "Fetching first chunk in GrpcStore::read()")?;
        Ok(FirstStream {
            first_response: Some(first_response),
            stream: response,
        })
    }

    pub async fn read(
        &self,
        grpc_request: impl IntoRequest<ReadRequest>,
    ) -> Result<impl Stream<Item = Result<ReadResponse, Status>>, Error> {
        error_if!(
            matches!(self.store_type, nativelink_config::stores::StoreType::ac),
            "CAS operation on AC store"
        );

        let request = self.get_read_request(grpc_request.into_request().into_inner())?;
        self.perform_request(request, |request| async move { self.read_internal(request).await })
            .await
    }

    pub async fn write<T, E>(&self, stream: WriteRequestStreamWrapper<T, E>) -> Result<Response<WriteResponse>, Error>
    where
        T: Stream<Item = Result<WriteRequest, E>> + Unpin + Send + 'static,
        E: Into<Error> + 'static,
    {
        error_if!(
            matches!(self.store_type, nativelink_config::stores::StoreType::ac),
            "CAS operation on AC store"
        );

        let local_state = Arc::new(Mutex::new(WriteState::new(self.instance_name.clone(), stream)));

        let result = self
            .retrier
            .retry(unfold(local_state, move |local_state| async move {
                let (connection, channel) = self.connection_manager.get_connection().await;
                // The client write may occur on a separate thread and
                // therefore in order to share the state with it we have to
                // wrap it in a Mutex and retrieve it after the write
                // has completed.  There is no way to get the value back
                // from the client.
                let result = ByteStreamClient::new(channel)
                    .write(WriteStateWrapper {
                        shared_state: local_state.clone(),
                    })
                    .await;

                // Get the state back from StateWrapper, this should be
                // uncontended since write has returned.
                let mut local_state_locked = local_state.lock();

                let result = if let Some(err) = local_state_locked.read_stream_error.take() {
                    // If there was an error with the stream, then don't
                    // retry.
                    RetryResult::Err(err)
                } else {
                    // On error determine whether it is possible to retry.
                    match result.err_tip(|| "in GrpcStore::write") {
                        Err(err) => {
                            connection.on_error(&err);
                            if local_state_locked.can_resume() {
                                local_state_locked.resume();
                                RetryResult::Retry(err)
                            } else {
                                RetryResult::Err(err.append("Retry is not possible"))
                            }
                        }
                        Ok(response) => RetryResult::Ok(response),
                    }
                };

                drop(local_state_locked);
                Some((result, local_state))
            }))
            .await?;
        Ok(result)
    }

    pub async fn query_write_status(
        &self,
        grpc_request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Error> {
        error_if!(
            matches!(self.store_type, nativelink_config::stores::StoreType::ac),
            "CAS operation on AC store"
        );

        let mut request = grpc_request.into_inner();

        const IS_UPLOAD_TRUE: bool = true;
        let mut request_info = ResourceInfo::new(&request.resource_name, IS_UPLOAD_TRUE)?;
        if request_info.instance_name != self.instance_name {
            request_info.instance_name = &self.instance_name;
            request.resource_name = request_info.to_string(IS_UPLOAD_TRUE);
        }

        self.perform_request(request, |request| async move {
            let (connection, channel) = self.connection_manager.get_connection().await;
            let result = ByteStreamClient::new(channel)
                .query_write_status(Request::new(request))
                .await
                .err_tip(|| "in GrpcStore::query_write_status");
            if let Err(err) = &result {
                connection.on_error(err);
            }
            result
        })
        .await
    }

    pub async fn get_action_result(
        &self,
        grpc_request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Error> {
        let mut request = grpc_request.into_inner();
        request.instance_name = self.instance_name.clone();
        self.perform_request(request, |request| async move {
            let (connection, channel) = self.connection_manager.get_connection().await;
            let result = ActionCacheClient::new(channel)
                .get_action_result(Request::new(request))
                .await
                .err_tip(|| "in GrpcStore::get_action_result");
            if let Err(err) = &result {
                connection.on_error(err);
            }
            result
        })
        .await
    }

    pub async fn update_action_result(
        &self,
        grpc_request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Error> {
        let mut request = grpc_request.into_inner();
        request.instance_name = self.instance_name.clone();
        self.perform_request(request, |request| async move {
            let (connection, channel) = self.connection_manager.get_connection().await;
            let result = ActionCacheClient::new(channel)
                .update_action_result(Request::new(request))
                .await
                .err_tip(|| "in GrpcStore::update_action_result");
            if let Err(err) = &result {
                connection.on_error(err);
            }
            result
        })
        .await
    }

    async fn get_action_result_from_digest(&self, digest: DigestInfo) -> Result<Response<ActionResult>, Error> {
        let action_result_request = GetActionResultRequest {
            instance_name: self.instance_name.clone(),
            action_digest: Some(digest.into()),
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: Vec::new(),
            digest_function: digest_function::Value::Sha256.into(),
        };
        self.get_action_result(Request::new(action_result_request)).await
    }

    async fn get_action_result_as_part(
        &self,
        digest: DigestInfo,
        writer: &mut DropCloserWriteHalf,
        offset: usize,
        length: Option<usize>,
    ) -> Result<(), Error> {
        let action_result = self
            .get_action_result_from_digest(digest)
            .await
            .map(|response| response.into_inner())
            .err_tip(|| "Action result not found")?;
        // TODO: Would be better to avoid all the encoding and decoding in this
        //       file, however there's no way to currently get raw bytes from a
        //       generated prost request unfortunately.
        let mut value = BytesMut::new();
        action_result
            .encode(&mut value)
            .err_tip(|| "Could not encode upstream action result")?;

        let default_len = value.len() - offset;
        let length = length.unwrap_or(default_len).min(default_len);
        if length > 0 {
            writer
                .send(value.freeze().slice(offset..(offset + length)))
                .await
                .err_tip(|| "Failed to write data in grpc store")?;
        }
        writer
            .send_eof()
            .await
            .err_tip(|| "Failed to write EOF in grpc store get_action_result_as_part")?;
        Ok(())
    }

    async fn update_action_result_from_bytes(
        &self,
        digest: DigestInfo,
        reader: DropCloserReadHalf,
    ) -> Result<(), Error> {
        let action_result = ActionResult::decode(reader.collect_all_with_size_hint(ESTIMATED_DIGEST_SIZE).await?)
            .err_tip(|| "Failed to decode ActionResult in update_action_result_from_bytes")?;
        let update_action_request = UpdateActionResultRequest {
            instance_name: self.instance_name.clone(),
            action_digest: Some(digest.into()),
            action_result: Some(action_result),
            results_cache_policy: None,
            digest_function: digest_function::Value::Sha256.into(),
        };
        self.update_action_result(Request::new(update_action_request))
            .await
            .map(|_| ())
    }
}

#[async_trait]
impl Store for GrpcStore {
    // NOTE: This function can only be safely used on CAS stores. AC stores may return a size that
    // is incorrect.
    async fn has_with_results(
        self: Pin<&Self>,
        digests: &[DigestInfo],
        results: &mut [Option<usize>],
    ) -> Result<(), Error> {
        if matches!(self.store_type, nativelink_config::stores::StoreType::ac) {
            digests
                .iter()
                .zip(results.iter_mut())
                .map(|(digest, result)| async move {
                    // The length of an AC is incorrect, so we don't figure out the
                    // length, instead the biggest possible result is returned in the
                    // hope that we detect incorrect usage.
                    self.get_action_result_from_digest(*digest).await?;
                    *result = Some(usize::MAX);
                    Ok::<_, Error>(())
                })
                .collect::<FuturesUnordered<_>>()
                .try_for_each(|_| future::ready(Ok(())))
                .await
                .err_tip(|| "Getting upstream action cache entry")?;
            return Ok(());
        }

        let missing_blobs_response = self
            .find_missing_blobs(Request::new(FindMissingBlobsRequest {
                instance_name: self.instance_name.clone(),
                blob_digests: digests.iter().map(|digest| digest.into()).collect(),
                digest_function: digest_function::Value::Sha256.into(),
            }))
            .await?
            .into_inner();

        // Since the ordering is not guaranteed above, the matching has to check
        // all missing blobs against all entries in the unsorted digest list.
        // To optimise this, the missing digests are sorted and then it is
        // efficient to perform a binary search for each digest within the
        // missing list.
        let mut missing_digests = Vec::with_capacity(missing_blobs_response.missing_blob_digests.len());
        for missing_digest in missing_blobs_response.missing_blob_digests {
            missing_digests.push(DigestInfo::try_from(missing_digest)?);
        }
        missing_digests.sort_unstable();
        for (digest, result) in digests.iter().zip(results.iter_mut()) {
            match missing_digests.binary_search(digest) {
                Ok(_) => *result = None,
                Err(_) => *result = Some(usize::try_from(digest.size_bytes)?),
            }
        }

        Ok(())
    }

    async fn update(
        self: Pin<&Self>,
        digest: DigestInfo,
        reader: DropCloserReadHalf,
        _size_info: UploadSizeInfo,
    ) -> Result<(), Error> {
        if matches!(self.store_type, nativelink_config::stores::StoreType::ac) {
            return self.update_action_result_from_bytes(digest, reader).await;
        }

        let mut buf = Uuid::encode_buffer();
        let resource_name = format!(
            "{}/uploads/{}/blobs/{}/{}",
            &self.instance_name,
            Uuid::new_v4().hyphenated().encode_lower(&mut buf),
            digest.hash_str(),
            digest.size_bytes,
        );

        struct LocalState {
            resource_name: String,
            reader: DropCloserReadHalf,
            did_error: bool,
            bytes_received: i64,
        }
        let local_state = LocalState {
            resource_name,
            reader,
            did_error: false,
            bytes_received: 0,
        };

        let stream = Box::pin(unfold(local_state, |mut local_state| async move {
            if local_state.did_error {
                error!("GrpcStore::update() polled stream after error was returned.");
                return None;
            }
            let data = match local_state.reader.recv().await.err_tip(|| "In GrpcStore::update()") {
                Ok(data) => data,
                Err(err) => {
                    local_state.did_error = true;
                    return Some((Err(err), local_state));
                }
            };

            let write_offset = local_state.bytes_received;
            local_state.bytes_received += data.len() as i64;

            Some((
                Ok(WriteRequest {
                    resource_name: local_state.resource_name.clone(),
                    write_offset,
                    finish_write: data.is_empty(), // EOF is when no data was polled.
                    data,
                }),
                local_state,
            ))
        }));

        self.write(
            WriteRequestStreamWrapper::from(stream)
                .await
                .err_tip(|| "in GrpcStore::update()")?,
        )
        .await
        .err_tip(|| "in GrpcStore::update()")?;

        Ok(())
    }

    async fn get_part_ref(
        self: Pin<&Self>,
        digest: DigestInfo,
        writer: &mut DropCloserWriteHalf,
        offset: usize,
        length: Option<usize>,
    ) -> Result<(), Error> {
        if matches!(self.store_type, nativelink_config::stores::StoreType::ac) {
            return self.get_action_result_as_part(digest, writer, offset, length).await;
        }

        // Shortcut for empty blobs.
        if digest.size_bytes == 0 {
            return writer.send_eof().await;
        }

        let resource_name = format!(
            "{}/blobs/{}/{}",
            &self.instance_name,
            digest.hash_str(),
            digest.size_bytes,
        );

        struct LocalState<'a> {
            resource_name: String,
            writer: &'a mut DropCloserWriteHalf,
            read_offset: i64,
            read_limit: i64,
        }

        let local_state = LocalState {
            resource_name,
            writer,
            read_offset: offset as i64,
            read_limit: length.unwrap_or(0) as i64,
        };

        self.retrier
            .retry(unfold(local_state, move |mut local_state| async move {
                let request = ReadRequest {
                    resource_name: local_state.resource_name.clone(),
                    read_offset: local_state.read_offset,
                    read_limit: local_state.read_limit,
                };
                let mut stream = match self.read_internal(request).await.err_tip(|| "in GrpcStore::get_part()") {
                    Ok(stream) => stream,
                    Err(err) => return Some((RetryResult::Retry(err), local_state)),
                };

                loop {
                    let data = match stream.next().await {
                        // Create an empty response to represent EOF.
                        None => bytes::Bytes::new(),
                        Some(Ok(message)) => message.data,
                        Some(Err(status)) => {
                            return Some((
                                RetryResult::Retry(
                                    Into::<Error>::into(status)
                                        .append("While fetching message in GrpcStore::get_part()"),
                                ),
                                local_state,
                            ))
                        }
                    };
                    let length = data.len() as i64;
                    // This is the usual exit from the loop at EOF.
                    if length == 0 {
                        let eof_result = local_state
                            .writer
                            .send_eof()
                            .await
                            .err_tip(|| "Could not send eof in GrpcStore::get_part()")
                            .map_or_else(RetryResult::Err, RetryResult::Ok);
                        return Some((eof_result, local_state));
                    }
                    // Forward the data upstream.
                    if let Err(err) = local_state
                        .writer
                        .send(data)
                        .await
                        .err_tip(|| "While sending in GrpcStore::get_part()")
                    {
                        return Some((RetryResult::Err(err), local_state));
                    }
                    local_state.read_offset += length;
                }
            }))
            .await
    }

    fn inner_store(self: Arc<Self>, _digest: Option<DigestInfo>) -> Arc<dyn Store> {
        self
    }

    fn as_any(self: Arc<Self>) -> Box<dyn std::any::Any + Send> {
        Box::new(self)
    }
}

default_health_status_indicator!(GrpcStore);
