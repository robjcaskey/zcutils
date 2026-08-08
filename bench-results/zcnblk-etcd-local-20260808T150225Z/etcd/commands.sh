taskset -c 4-7\,12-15 /tmp/zcutils-etcd-bin/etcd --data-dir /mnt/zc-fs-app-bench/zcutils-etcd-data.WMDKSk 
env -u ETCDCTL_BIN /tmp/zcutils-etcd-bin/etcdctl --endpoints=http://127.0.0.1:22379 put zcutils-bench-key benchmark-value 
taskset -c 0\,8\,16\,24 /tmp/zcutils-etcd-bin/benchmark --endpoints 127.0.0.1:22379 --clients 32 --conns 8 --precise put --total 20000 --key-size 16 --key-space-size 20000 --val-size 256 --sequential-keys 
