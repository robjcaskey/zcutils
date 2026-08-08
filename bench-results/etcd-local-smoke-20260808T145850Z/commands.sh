taskset -c 0-3 /tmp/zcutils-etcd-bin/etcd --data-dir /tmp/zcutils-etcd-data.oxF66k 
env -u ETCDCTL_BIN /tmp/zcutils-etcd-bin/etcdctl --endpoints=http://127.0.0.1:22379 put zcutils-bench-key benchmark-value 
taskset -c 4-11 /tmp/zcutils-etcd-bin/benchmark --endpoints 127.0.0.1:22379 --clients 16 --conns 4 --precise put --total 10000 --key-size 16 --key-space-size 10000 --val-size 256 --sequential-keys 
taskset -c 4-11 /tmp/zcutils-etcd-bin/benchmark --endpoints 127.0.0.1:22379 --clients 16 --conns 4 --precise range --total 10000 --consistency l --limit 1 zcutils-bench-key 
taskset -c 4-11 /tmp/zcutils-etcd-bin/benchmark --endpoints 127.0.0.1:22379 --clients 16 --conns 4 --precise txn-mixed --total 5000 --rw-ratio 1 --consistency l --limit 1 --key-size 16 --key-space-size 10000 --val-size 256 zcutils-bench-key 
