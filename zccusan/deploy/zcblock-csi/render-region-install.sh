#!/usr/bin/env bash
set -euo pipefail

REGION="${1:?usage: render-region-install.sh <region>}"
IMAGE="${IMAGE:-localhost/zcblock-csi:dev}"
FREEZE_MAX_TTL_MS="${FREEZE_MAX_TTL_MS:-5000}"
SNAPSHOT_MODE="${SNAPSHOT_MODE:-auto}"
RAW_PARTUUID="${RAW_PARTUUID:-6dfb2c34-e1a4-4cd5-a4f6-d82bfadcd363}"

case "$REGION" in
  *[!a-z0-9-]* | "" )
    echo "region must contain only lowercase letters, digits, and '-': $REGION" >&2
    exit 1
    ;;
esac

NAME="zcblock-csi-${REGION}"
DRIVER="io.zcutils.zcblock.${REGION}"
STATE_DIR="/var/lib/${NAME}"
PLUGIN_DIR="/var/lib/kubelet/plugins/${DRIVER}"

cat <<YAML
apiVersion: v1
kind: Namespace
metadata:
  name: ${NAME}
  labels:
    zcutils.io/local-region: "${REGION}"
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: ${NAME}
  namespace: ${NAME}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: ${NAME}-provisioner
rules:
  - apiGroups: [""]
    resources: ["persistentvolumes"]
    verbs: ["get", "list", "watch", "create", "delete", "patch", "update"]
  - apiGroups: [""]
    resources: ["persistentvolumeclaims"]
    verbs: ["get", "list", "watch", "patch", "update"]
  - apiGroups: [""]
    resources: ["events"]
    verbs: ["get", "list", "watch", "create", "patch", "update"]
  - apiGroups: [""]
    resources: ["nodes"]
    verbs: ["get", "list", "watch"]
  - apiGroups: ["storage.k8s.io"]
    resources: ["storageclasses", "csinodes"]
    verbs: ["get", "list", "watch"]
  - apiGroups: ["snapshot.storage.k8s.io"]
    resources: ["volumesnapshotclasses", "volumesnapshots"]
    verbs: ["get", "list", "watch"]
  - apiGroups: ["snapshot.storage.k8s.io"]
    resources: ["volumesnapshotcontents"]
    verbs: ["get", "list", "watch", "update", "patch"]
  - apiGroups: ["snapshot.storage.k8s.io"]
    resources: ["volumesnapshotcontents/status"]
    verbs: ["update", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: ${NAME}-provisioner
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: ${NAME}-provisioner
subjects:
  - kind: ServiceAccount
    name: ${NAME}
    namespace: ${NAME}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: ${NAME}-leases
  namespace: ${NAME}
rules:
  - apiGroups: ["coordination.k8s.io"]
    resources: ["leases"]
    verbs: ["get", "list", "watch", "create", "delete", "patch", "update"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: ${NAME}-leases
  namespace: ${NAME}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: ${NAME}-leases
subjects:
  - kind: ServiceAccount
    name: ${NAME}
    namespace: ${NAME}
---
apiVersion: storage.k8s.io/v1
kind: CSIDriver
metadata:
  name: ${DRIVER}
  labels:
    zcutils.io/local-region: "${REGION}"
spec:
  attachRequired: false
  fsGroupPolicy: File
  podInfoOnMount: false
  storageCapacity: false
  volumeLifecycleModes:
    - Persistent
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: ${NAME}-node
  namespace: ${NAME}
  labels:
    app.kubernetes.io/name: zcblock-csi
    zcutils.io/local-region: "${REGION}"
spec:
  selector:
    matchLabels:
      app.kubernetes.io/name: zcblock-csi
      zcutils.io/local-region: "${REGION}"
  template:
    metadata:
      labels:
        app.kubernetes.io/name: zcblock-csi
        zcutils.io/local-region: "${REGION}"
    spec:
      serviceAccountName: ${NAME}
      priorityClassName: system-node-critical
      tolerations:
        - operator: Exists
      containers:
        - name: zcblock-csi
          image: ${IMAGE}
          imagePullPolicy: IfNotPresent
          args:
            - --driver-name=${DRIVER}
            - --endpoint=unix:///csi/csi.sock
            - --node-id=\$(NODE_NAME)
            - --state-dir=${STATE_DIR}
            - --control-socket=${STATE_DIR}/control.sock
            - --control-url=http://127.0.0.1:9788
            - --freeze-max-ttl-ms=${FREEZE_MAX_TTL_MS}
            - --snapshot-mode=${SNAPSHOT_MODE}
          env:
            - name: NODE_NAME
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
          securityContext:
            privileged: true
            allowPrivilegeEscalation: true
          volumeMounts:
            - name: plugin-dir
              mountPath: /csi
            - name: state-dir
              mountPath: ${STATE_DIR}
            - name: kubelet-dir
              mountPath: /var/lib/kubelet
              mountPropagation: Bidirectional
            - name: configfs
              mountPath: /sys/kernel/config
            - name: dev
              mountPath: /dev
        - name: zcblock-control
          image: ${IMAGE}
          imagePullPolicy: IfNotPresent
          command:
            - /usr/local/bin/zcblock-control
          args:
            - --listen=127.0.0.1:9788
            - --state-dir=${STATE_DIR}
            - --freeze-max-ttl-ms=${FREEZE_MAX_TTL_MS}
            - --snapshot-mode=${SNAPSHOT_MODE}
          securityContext:
            privileged: true
            allowPrivilegeEscalation: true
          volumeMounts:
            - name: state-dir
              mountPath: ${STATE_DIR}
            - name: kubelet-dir
              mountPath: /var/lib/kubelet
              mountPropagation: Bidirectional
            - name: configfs
              mountPath: /sys/kernel/config
            - name: dev
              mountPath: /dev
        - name: csi-provisioner
          image: registry.k8s.io/sig-storage/csi-provisioner:v5.3.0
          imagePullPolicy: IfNotPresent
          args:
            - --csi-address=/csi/csi.sock
            - --node-deployment
            - --node-deployment-immediate-binding=false
            - --strict-topology
            - --leader-election-namespace=\$(POD_NAMESPACE)
            - --timeout=60s
            - --v=3
          env:
            - name: NODE_NAME
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
            - name: POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
          volumeMounts:
            - name: plugin-dir
              mountPath: /csi
        - name: csi-snapshotter
          image: registry.k8s.io/sig-storage/csi-snapshotter:v8.3.0
          imagePullPolicy: IfNotPresent
          args:
            - --csi-address=/csi/csi.sock
            - --node-deployment
            - --leader-election-namespace=\$(POD_NAMESPACE)
            - --timeout=600s
            - --extra-create-metadata
            - --v=3
          env:
            - name: NODE_NAME
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
            - name: POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
          volumeMounts:
            - name: plugin-dir
              mountPath: /csi
        - name: node-driver-registrar
          image: registry.k8s.io/sig-storage/csi-node-driver-registrar:v2.16.0
          imagePullPolicy: IfNotPresent
          args:
            - --csi-address=/csi/csi.sock
            - --kubelet-registration-path=${PLUGIN_DIR}/csi.sock
            - --v=3
          volumeMounts:
            - name: plugin-dir
              mountPath: /csi
            - name: registration-dir
              mountPath: /registration
      volumes:
        - name: plugin-dir
          hostPath:
            path: ${PLUGIN_DIR}
            type: DirectoryOrCreate
        - name: registration-dir
          hostPath:
            path: /var/lib/kubelet/plugins_registry
            type: DirectoryOrCreate
        - name: state-dir
          hostPath:
            path: ${STATE_DIR}
            type: DirectoryOrCreate
        - name: kubelet-dir
          hostPath:
            path: /var/lib/kubelet
            type: Directory
        - name: configfs
          hostPath:
            path: /sys/kernel/config
            type: Directory
        - name: dev
          hostPath:
            path: /dev
            type: Directory
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: zcbrd-${REGION}
  labels:
    zcutils.io/local-region: "${REGION}"
provisioner: ${DRIVER}
reclaimPolicy: Delete
allowVolumeExpansion: false
volumeBindingMode: WaitForFirstConsumer
parameters:
  backend: zcbrd
  blocksize: "4096"
  queues: "8"
  queueDepth: "512"
  descriptorMode: advertise
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: zcfile-${REGION}
  labels:
    zcutils.io/local-region: "${REGION}"
provisioner: ${DRIVER}
reclaimPolicy: Delete
allowVolumeExpansion: false
volumeBindingMode: WaitForFirstConsumer
parameters:
  backend: file-loop
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: zcraw-${REGION}
  labels:
    zcutils.io/local-region: "${REGION}"
  annotations:
    zcutils.io/raw-block-warning: "Local multi-region simulation only. Do not bind the same raw partition through multiple regions concurrently."
provisioner: ${DRIVER}
reclaimPolicy: Delete
allowVolumeExpansion: false
volumeBindingMode: WaitForFirstConsumer
parameters:
  backend: raw-block
  rawPartUUID: "${RAW_PARTUUID}"
---
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshotClass
metadata:
  name: zcblock-${REGION}
  labels:
    zcutils.io/local-region: "${REGION}"
  annotations:
    zcutils.io/snapshot-crd-storage-version: "v1"
    zcutils.io/snapshot-crd-stair-step-required: "true"
driver: ${DRIVER}
deletionPolicy: Delete
YAML
