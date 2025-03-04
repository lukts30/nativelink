# Get the nix derivation hash from the toolchain container, change the
# `TOOLCHAIN_TAG` variable in the `worker.json.template` to that hash and apply
# the configuration.

KUSTOMIZE_DIR=$(git rev-parse --show-toplevel)/deployment-examples/kubernetes

sed "s/__NATIVELINK_TOOLCHAIN_TAG__/$(nix eval .#lre.imageTag --raw)/g" \
  "$KUSTOMIZE_DIR/worker.json.template" \
  > "$KUSTOMIZE_DIR/worker.json"

kubectl apply -k "$KUSTOMIZE_DIR"

kubectl rollout status deploy/nativelink-cas
kubectl rollout status deploy/nativelink-scheduler
kubectl rollout status deploy/nativelink-worker
