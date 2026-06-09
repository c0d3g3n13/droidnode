# DroidNode

Turn Android phones into Kubernetes worker nodes — no root required.

Standard `kubectl apply` workloads run on mobile SoCs. The control plane (k3s or vanilla k8s) needs no modification. From the scheduler's perspective a DroidNode device is an ordinary Linux worker node.

```
$ kubectl get nodes
NAME                        STATUS   ROLES    AGE
kali                        Ready    master   4d
droidnode-b96a49db497af1eb  Ready    <none>   2h   ← Android phone

$ kubectl apply -f workload.yaml
$ kubectl logs my-pod
hello from DroidNode
architecture: aarch64
```

---

## How it works

1. The **Android APK** starts a foreground service that spawns the Rust agent as a native subprocess.
2. The **Rust agent** registers the device with k3s as a virtual kubelet node and opens an HTTPS endpoint that implements the Kubernetes kubelet API.
3. When k3s schedules a pod, the agent pulls the OCI image layers, unpacks them into a rootfs, and runs the container via **proot** — a ptrace-based userspace chroot that requires no root.
4. Pod status and logs flow back to the control plane in real time.

**No root. No custom kernel modules. No k3s patches.**

---

## Architecture

DroidNode follows [The Standard](https://github.com/hassanhabib/The-Standard) (Hassan Habib) strictly:

```
Brokers → Foundation Services → Orchestration Services → Exposers
```

```
agent/
├── executor/src/
│   ├── brokers/
│   │   ├── oci_registry_broker.rs      # Pull OCI manifests and layers
│   │   ├── filesystem_broker.rs        # Layer storage on-device
│   │   ├── proot_broker.rs             # Spawn proot containers
│   │   └── control_plane_broker.rs     # k8s API client
│   ├── services/foundation/
│   │   ├── image_pull_service.rs       # Fetch + cache OCI layers
│   │   ├── image_unpack_service.rs     # Unpack tarballs to rootfs
│   │   ├── workload_execution_service.rs
│   │   ├── health_probe_service.rs
│   │   ├── node_capability_service.rs
│   │   └── event_recording_service.rs
│   ├── services/orchestration/
│   │   ├── image_orchestration_service.rs  # Pull → unpack → rootfs
│   │   ├── reconciliation_service.rs       # Desired vs actual loop (15 s)
│   │   ├── workload_lifecycle_service.rs   # Start / stop / status
│   │   └── node_registration_service.rs    # Register + heartbeat
│   └── exposers/
│       └── virtual_kubelet_exposer.rs  # HTTPS kubelet API (:10250)
└── node-agent/src/main.rs              # Entry point + config

android/
└── app/src/main/java/com/droidnode/
    ├── brokers/           # Android system brokers (process, wake lock, battery, network)
    ├── services/          # AgentLifecycleService, ResourceGuardService
    ├── services/orchestration/  # NodeReadinessService
    ├── exposers/          # ForegroundServiceExposer (Android foreground service)
    ├── ui/                # DebugActivity — on-device LOGS / STATUS / PODS UI
    └── LogBuffer.kt       # Ring buffer bridging agent stdout to the debug UI
```

---

## Prerequisites

| Tool | Version | Notes |
|------|---------|-------|
| Rust + Cargo | stable | `rustup target add aarch64-linux-android` |
| Android NDK | r27c+ | Set `ANDROID_NDK_HOME` or let the script download it |
| Android SDK | API 26+ | Android Studio or `sdkmanager` |
| Java | 17 | For Gradle |
| `patchelf` | any | `sudo apt install patchelf` |
| `adb` | any | For sideloading |
| k3s | v1.28+ | On the control-plane machine |

---

## Build

### 1 — Rust agent + proot binaries

```bash
./scripts/build-android.sh
```

This will:
- Download NDK r27c if `ANDROID_NDK_HOME` is not set
- Cross-compile `node-agent` for `aarch64-linux-android`
- Download Termux proot + libtalloc, patch RPATH/NEEDED for Android
- Extract the proot ELF loader and rename it `libproot_loader.so`
- Copy all four `.so` files into `android/app/src/main/jniLibs/arm64-v8a/`

### 2 — Android APK

```bash
cd android
./gradlew assembleDebug
# APK → android/app/build/outputs/apk/debug/app-debug.apk
```

Or open `android/` in Android Studio and click **Run**.

### 3 — Install

```bash
adb install android/app/build/outputs/apk/debug/app-debug.apk
```

---

## Setup

### Control plane (one-time)

```bash
# Create the RBAC service account the agent authenticates as
kubectl apply -f deploy/rbac.yaml

# Generate a kubeconfig scoped to the droidnode-agent service account
# (example using k3s — adjust for your cluster)
TOKEN=$(kubectl -n kube-system create token droidnode-agent)
kubectl config set-credentials droidnode-agent --token="$TOKEN"
kubectl config set-context droidnode --cluster=<your-cluster> --user=droidnode-agent
kubectl config use-context droidnode
kubectl config view --minify --flatten > /tmp/droidnode-kubeconfig
```

### TLS (one-time per device)

The agent generates a kubelet TLS certificate on first start. k3s must trust its CA:

```bash
# After first APK launch, copy the CA from the device
adb shell run-as com.droidnode cat /data/data/com.droidnode/files/kubelet-ca.crt \
    > /tmp/android-kubelet-ca.crt

# Trust it in k3s
sudo cp /tmp/android-kubelet-ca.crt /etc/rancher/k3s/
# Add to /etc/rancher/k3s/config.yaml:
#   kubelet-certificate-authority: /etc/rancher/k3s/android-kubelet-ca.crt
sudo systemctl restart k3s
```

### Push kubeconfig to device

```bash
adb push /tmp/droidnode-kubeconfig \
    /data/data/com.droidnode/files/kubeconfig
```

### Start the node

Launch the DroidNode app and tap **Start Agent**. The node appears in `kubectl get nodes` within ~30 seconds.

---

## Usage

### Schedule a pod

```yaml
# workload.yaml
apiVersion: v1
kind: Pod
metadata:
  name: hello-android
spec:
  nodeName: droidnode-<device-id>   # from kubectl get nodes
  restartPolicy: Never
  containers:
    - name: hello
      image: alpine:latest
      command: ["/bin/sh", "-c"]
      args: ["echo hello from DroidNode; uname -m"]
```

```bash
kubectl apply -f workload.yaml
kubectl logs hello-android
# hello from DroidNode
# aarch64
```

### On-device debug UI

Tap the persistent notification to open the debug UI:
- **LOGS** — live color-coded stream from the agent
- **STATUS** — uptime, node ID, running state
- **PODS** — pod lifecycle events parsed in real time

---

## Known limitations

See [open issues](../../issues) for the full list. Major gaps today:

| Area | Status |
|------|--------|
| Multi-container pods | Only the first container runs |
| `restartPolicy: Always` | Not implemented — pods run once |
| Volume mounts | Not functional (source path mapping is wrong) |
| Resource limits | CPU/memory requests and limits are ignored |
| Graceful shutdown | Containers receive SIGKILL, no SIGTERM grace period |
| Load balancer pods (svclb) | iptables calls fail inside proot |
| Network isolation | Containers share the host network namespace |
| `kubectl exec` / `kubectl port-forward` | Not implemented |
| ARM32 / x86 devices | Only `aarch64` tested |

---

## Contributing

1. Fork and clone the repo.
2. Run `./scripts/build-android.sh` to get the binaries.
3. Open `android/` in Android Studio for Kotlin work; the Rust crates are in `agent/`.
4. Run Rust checks locally before pushing:
   ```bash
   cd agent && cargo check --workspace && cargo clippy --workspace
   ```
5. Open a PR — CI runs `cargo check`, `cargo clippy`, and `cargo test` automatically.

Architecture rule: **no layer skips**. Brokers call external systems only. Foundation services do one thing. Orchestration services coordinate. Exposers are thin entry points. PRs that skip layers will be asked to refactor.

---

## License

MIT
