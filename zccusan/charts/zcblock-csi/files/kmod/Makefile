KDIR ?= /lib/modules/$(shell uname -r)/build

obj-m += zcnblk_client_mod.o
ccflags-y += -Wall

.PHONY: all clean

all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean
