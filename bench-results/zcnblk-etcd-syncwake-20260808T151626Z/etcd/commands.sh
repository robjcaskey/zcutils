taskset -c 4-7\,12-15 /tmp/zcutils-etcd-bin/etcd --data-dir /mnt/zc-fs-app-bench/zcutils-etcd-data.jlrV6s 
env -u ETCDCTL_BIN /tmp/zcutils-etcd-bin/etcdctl --endpoints=http://127.0.0.1:22379 put zcutils-bench-key benchmark-value 
taskset -c 0\,8\,16\,24 /tmp/zcutils-etcd-bin/benchmark --endpoints 127.0.0.1:22379 --clients 32 --conns 8 --precise put --total 2000 --key-size 16 --key-space-size 2000 --val-size 256 --sequential-keys 
taskset -c 0\,8\,16\,24 /tmp/zcutils-etcd-bin/benchmark --endpoints 127.0.0.1:22379 --clients 32 --conns 8 --precise range --total 5000 --consistency l --limit 1 zcutils-bench-key 
taskset -c 0\,8\,16\,24 /tmp/zcutils-etcd-bin/benchmark --endpoints 127.0.0.1:22379 --clients 32 --conns 8 --precise txn-mixed --total 1000 --rw-ratio 1 --consistency l --limit 1 --key-size 16 --key-space-size 2000 --val-size 256 zcutils-bench-key 
