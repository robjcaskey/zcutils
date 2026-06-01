KDIR ?= /lib/modules/$(shell uname -r)/build

obj-m += zcbrd_mod.o

ccflags-y += -Wno-declaration-after-statement

.PHONY: all clean

all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean
