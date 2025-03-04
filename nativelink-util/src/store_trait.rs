// Copyright 2023 The NativeLink Authors. All rights reserved.
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

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher as StdHasher;
use std::hash::{Hash, Hasher};
use std::marker::Send;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{join, try_join};
// use lru::DefaultHasher;
use nativelink_error::{make_err, Code, Error, ResultExt};
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::buf_channel::{make_buf_channel_pair, DropCloserReadHalf, DropCloserWriteHalf};
use crate::common::DigestInfo;
use crate::digest_hasher::{default_digest_hasher_func, DigestHasher};
use crate::health_utils::{HealthRegistryBuilder, HealthStatus, HealthStatusIndicator};
use crate::metrics_utils::Registry;

static DEFAULT_DIGEST_SIZE_HEALTH_CHECK: OnceLock<usize> = OnceLock::new();
/// Default digest size for health check data. Any change in this value
/// changes the default contract. `GlobalConfig` should be updated to reflect
/// changes in this value.
pub const DEFAULT_DIGEST_SIZE_HEALTH_CHECK_CFG: usize = 1024 * 1024;

// Get the default digest size for health check data, if value is unset a system wide default is used.
pub fn default_digest_size_health_check() -> usize {
    *DEFAULT_DIGEST_SIZE_HEALTH_CHECK.get_or_init(|| DEFAULT_DIGEST_SIZE_HEALTH_CHECK_CFG)
}

/// Set the default digest size for health check data, this should be called once.
pub fn set_default_digest_size_health_check(size: usize) -> Result<(), Error> {
    DEFAULT_DIGEST_SIZE_HEALTH_CHECK
        .set(size)
        .map_err(|_| make_err!(Code::Internal, "set_default_digest_size_health_check already set"))
}

#[derive(Debug, PartialEq, Copy, Clone, Serialize, Deserialize)]
pub enum UploadSizeInfo {
    /// When the data transfer amount is known to be exact size, this enum should be used.
    /// The receiver store can use this to better optimize the way the data is sent or stored.
    ExactSize(usize),

    /// When the data transfer amount is not known to be exact, the caller should use this enum
    /// to provide the maximum size that could possibly be sent. This will bypass the exact size
    /// checks, but still provide useful information to the underlying store about the data being
    /// sent that it can then use to optimize the upload process.
    MaxSize(usize),
}

#[async_trait]
pub trait Store: Sync + Send + Unpin + HealthStatusIndicator {
    /// Look up a digest in the store and return None if it does not exist in
    /// the store, or Some(size) if it does.
    /// Note: On an AC store the size will be incorrect and should not be used!
    #[inline]
    async fn has(self: Pin<&Self>, digest: DigestInfo) -> Result<Option<usize>, Error> {
        let mut result = [None];
        self.has_with_results(&[digest], &mut result).await?;
        Ok(result[0])
    }

    /// Look up a list of digests in the store and return a result for each in
    /// the same order as input.  The result will either be None if it does not
    /// exist in the store, or Some(size) if it does.
    /// Note: On an AC store the size will be incorrect and should not be used!
    #[inline]
    async fn has_many(self: Pin<&Self>, digests: &[DigestInfo]) -> Result<Vec<Option<usize>>, Error> {
        let mut results = vec![None; digests.len()];
        self.has_with_results(digests, &mut results).await?;
        Ok(results)
    }

    /// The implementation of the above has and has_many functions.  See their
    /// documentation for details.
    async fn has_with_results(
        self: Pin<&Self>,
        digests: &[DigestInfo],
        results: &mut [Option<usize>],
    ) -> Result<(), Error>;

    async fn update(
        self: Pin<&Self>,
        digest: DigestInfo,
        reader: DropCloserReadHalf,
        upload_size: UploadSizeInfo,
    ) -> Result<(), Error>;

    // Utility to send all the data to the store when you have all the bytes.
    async fn update_oneshot(self: Pin<&Self>, digest: DigestInfo, data: Bytes) -> Result<(), Error> {
        // TODO(blaise.bruer) This is extremely inefficient, since we have exactly
        // what we need here. Maybe we could instead make a version of the stream
        // that can take objects already fully in memory instead?
        let (mut tx, rx) = make_buf_channel_pair();

        let data_len = data.len();
        let send_fut = async move {
            // Only send if we are not EOF.
            if !data.is_empty() {
                tx.send(data)
                    .await
                    .err_tip(|| "Failed to write data in update_oneshot")?;
            }
            tx.send_eof()
                .await
                .err_tip(|| "Failed to write EOF in update_oneshot")?;
            Ok(())
        };
        try_join!(send_fut, self.update(digest, rx, UploadSizeInfo::ExactSize(data_len)))?;
        Ok(())
    }

    /// Retreives part of the data from the store and writes it to the given writer.
    async fn get_part_ref(
        self: Pin<&Self>,
        digest: DigestInfo,
        writer: &mut DropCloserWriteHalf,
        offset: usize,
        length: Option<usize>,
    ) -> Result<(), Error>;

    /// Same as `get_part_ref`, but takes ownership of the writer. This is preferred
    /// when the writer is definitly not going to be needed after the function returns.
    /// This is useful because the read half of the writer will block until the writer
    /// is dropped or EOF is sent. If the writer was passed as a reference, and the
    /// reader was being waited with the `.get_part()`, it could deadlock if the writer
    /// is not dropped or EOF sent. `.get_part_ref()` should be used when the writer
    /// might be used after the function returns.
    #[inline]
    async fn get_part(
        self: Pin<&Self>,
        digest: DigestInfo,
        mut writer: DropCloserWriteHalf,
        offset: usize,
        length: Option<usize>,
    ) -> Result<(), Error> {
        self.get_part_ref(digest, &mut writer, offset, length).await
    }

    /// Utility that works the same as ``.get_part()`, but writes all the data.
    #[inline]
    async fn get(self: Pin<&Self>, digest: DigestInfo, writer: DropCloserWriteHalf) -> Result<(), Error> {
        self.get_part(digest, writer, 0, None).await
    }

    /// Utility for when `self` is an `Arc` to make the code easier to write.
    #[inline]
    async fn get_part_arc(
        self: Arc<Self>,
        digest: DigestInfo,
        writer: DropCloserWriteHalf,
        offset: usize,
        length: Option<usize>,
    ) -> Result<(), Error> {
        Pin::new(self.as_ref()).get_part(digest, writer, offset, length).await
    }

    // Utility that will return all the bytes at once instead of in a streaming manner.
    async fn get_part_unchunked(
        self: Pin<&Self>,
        digest: DigestInfo,
        offset: usize,
        length: Option<usize>,
        size_hint: Option<usize>,
    ) -> Result<Bytes, Error> {
        // TODO(blaise.bruer) This is extremely inefficient, since we have exactly
        // what we need here. Maybe we could instead make a version of the stream
        // that can take objects already fully in memory instead?
        let (tx, rx) = make_buf_channel_pair();

        let (data_res, get_part_res) = join!(
            rx.collect_all_with_size_hint(length.unwrap_or(size_hint.unwrap_or(0))),
            self.get_part(digest, tx, offset, length),
        );
        get_part_res
            .err_tip(|| "Failed to get_part in get_part_unchunked")
            .merge(data_res.err_tip(|| "Failed to read stream to completion in get_part_unchunked"))
    }

    // Default implementation of the health check. Some stores may want to override this
    // in situations where the default implementation is not sufficient.
    async fn check_health(self: Pin<&Self>, namespace: Cow<'static, str>) -> HealthStatus {
        let digest_data_size = default_digest_size_health_check();
        let mut digest_data = vec![0u8; digest_data_size];

        let mut namespace_hasher = StdHasher::new();
        namespace.hash(&mut namespace_hasher);
        self.get_name().hash(&mut namespace_hasher);
        let hash_seed = namespace_hasher.finish();

        // Fill the digest data with random data based on a stable
        // hash of the namespace and store name. Intention is to
        // have randomly filled data that is unique per store and
        // does not change between health checks. This is to ensure
        // we are not adding more data to store on each health check.
        let mut rng: StdRng = StdRng::seed_from_u64(hash_seed);
        rng.fill_bytes(&mut digest_data);

        let mut digest_hasher = DigestHasher::from(default_digest_hasher_func());
        digest_hasher.update(&digest_data);
        let digest_data_len = digest_data.len();
        let digest_info = digest_hasher.finalize_digest(digest_data_len as i64);

        let digest_bytes = bytes::Bytes::copy_from_slice(&digest_data);

        if let Err(e) = self.update_oneshot(digest_info, digest_bytes.clone()).await {
            return HealthStatus::new_failed(self.get_ref(), format!("Store.update_oneshot() failed: {}", e).into());
        }

        match self.has(digest_info).await {
            Ok(Some(s)) => {
                if s != digest_data_len {
                    return HealthStatus::new_failed(
                        self.get_ref(),
                        format!("Store.has() size mismatch {s} != {digest_data_len}").into(),
                    );
                }
            }
            Ok(None) => {
                return HealthStatus::new_failed(self.get_ref(), "Store.has() size not found".into());
            }
            Err(e) => {
                return HealthStatus::new_failed(self.get_ref(), format!("Store.has() failed: {}", e).into());
            }
        }

        match self
            .get_part_unchunked(digest_info, 0, Some(digest_data_len), Some(digest_data_len))
            .await
        {
            Ok(b) => {
                if b != digest_bytes {
                    return HealthStatus::new_failed(self.get_ref(), "Store.get_part_unchunked() data mismatch".into());
                }
            }
            Err(e) => {
                return HealthStatus::new_failed(
                    self.get_ref(),
                    format!("Store.get_part_unchunked() failed: {}", e).into(),
                );
            }
        }

        HealthStatus::new_ok(self.get_ref(), "Successfully store health check".into())
    }

    /// Gets the underlying store for the given digest. This can be used to find out
    /// what any underlying store is for a given digest will be and hand it to the caller.
    /// A caller might want to use this to obtain a reference to the "real" underlying store
    /// (if applicable) and check if it implements some special traits that allow optimizations.
    /// Note: If the store performs complex operations on the data, it should return itself.
    fn inner_store(self: Arc<Self>, _digest: Option<DigestInfo>) -> Arc<dyn Store>;

    /// Expect the returned Any to be `Arc<Self>`.
    fn as_any(self: Arc<Self>) -> Box<dyn std::any::Any + Send>;

    /// Register any metrics that this store wants to expose to the Prometheus.
    fn register_metrics(self: Arc<Self>, _registry: &mut Registry) {}

    // Register health checks used to monitor the store.
    fn register_health(self: Arc<Self>, _registry: &mut HealthRegistryBuilder) {}
}
