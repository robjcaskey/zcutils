# zcbrd/zcstripe C Modules

These out-of-tree modules provide C equivalents of the Rust `zcbrd_mod` and
`zcstripe_mod` prototypes so they can run on kernels that have the io-slot API
but were not built with `CONFIG_RUST=y`.

Build against the running kernel:

```sh
make -C kmods
```

On Secure Boot systems, sign the modules with an enrolled MOK before loading:

```sh
sudo /usr/src/linux-headers-$(uname -r)/scripts/sign-file sha256 \
  /root/mok/MOK.priv /root/mok/MOK.pem kmods/zcbrd_mod.ko
sudo /usr/src/linux-headers-$(uname -r)/scripts/sign-file sha256 \
  /root/mok/MOK.priv /root/mok/MOK.pem kmods/zcstripe_mod.ko
```

Load and create a pair of RAM block devices:

```sh
sudo insmod kmods/zcbrd_mod.ko
sudo mkdir /sys/kernel/config/zcbrd/zcbrd0
echo 256 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/size_mib
echo 4096 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/blocksize
echo 8 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/queues
echo 512 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/queue_depth
echo advertise | sudo tee /sys/kernel/config/zcbrd/zcbrd0/descriptor_mode
echo 1 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/power
```

Repeat with `zcbrd1` if you want a two-device stripe.

Create a stripe target across two lower devices:

```sh
sudo insmod kmods/zcstripe_mod.ko
sudo mkdir /sys/kernel/config/zcstripe/zcstripe0
echo /dev/zcbrd0,/dev/zcbrd1 | sudo tee /sys/kernel/config/zcstripe/zcstripe0/targets
echo 4096 | sudo tee /sys/kernel/config/zcstripe/zcstripe0/stripe_unit
echo 4096 | sudo tee /sys/kernel/config/zcstripe/zcstripe0/blocksize
echo 8 | sudo tee /sys/kernel/config/zcstripe/zcstripe0/queues
echo 512 | sudo tee /sys/kernel/config/zcstripe/zcstripe0/queue_depth
echo advertise | sudo tee /sys/kernel/config/zcstripe/zcstripe0/descriptor_mode
echo 1 | sudo tee /sys/kernel/config/zcstripe/zcstripe0/power
```

Both modules expose `descriptor_abi` through configfs and use blk-mq, so the
io-slot path accepts them on the `7.0.8-io-slots` kernel.
