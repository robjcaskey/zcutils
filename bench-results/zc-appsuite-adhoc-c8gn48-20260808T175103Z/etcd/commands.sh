taskset -c 99-107 /opt/etcd/etcd --data-dir /mnt/zc-fs-app-bench/zcutils-etcd-data.fGVDSr 
env -u ETCDCTL_BIN /opt/etcd/etcdctl --endpoints=http://127.0.0.1:22379 put zcutils-bench-key benchmark-value 
taskset -c 120-151 /opt/etcd/benchmark --endpoints 127.0.0.1:22379 --clients 64 --conns 8 --precise put --total 100000 --key-size 16 --key-space-size 100000 --val-size 256 --sequential-keys 
taskset -c 120-151 /opt/etcd/benchmark --endpoints 127.0.0.1:22379 --clients 64 --conns 8 --precise range --total 100000 --consistency l --limit 1 zcutils-bench-key 
taskset -c 120-151 /opt/etcd/benchmark --endpoints 127.0.0.1:22379 --clients 64 --conns 8 --precise txn-mixed --total 50000 --rw-ratio 1 --consistency l --limit 1 --key-size 16 --key-space-size 100000 --val-size 256 zcutils-bench-key 
