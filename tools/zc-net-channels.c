// SPDX-License-Identifier: GPL-2.0

#include <errno.h>
#include <linux/ethtool.h>
#include <linux/if.h>
#include <linux/sockios.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

static void usage(const char *program)
{
	fprintf(stderr, "usage: %s INTERFACE COMBINED-QUEUES\n", program);
}

static int get_channels(int fd, struct ifreq *request,
			struct ethtool_channels *channels)
{
	memset(channels, 0, sizeof(*channels));
	channels->cmd = ETHTOOL_GCHANNELS;
	request->ifr_data = (void *)channels;
	return ioctl(fd, SIOCETHTOOL, request);
}

int main(int argc, char **argv)
{
	struct ethtool_channels channels;
	struct ifreq request = { 0 };
	char *end = NULL;
	unsigned long requested;
	int fd;

	if (argc != 3) {
		usage(argv[0]);
		return 2;
	}
	if (strlen(argv[1]) >= IFNAMSIZ) {
		fprintf(stderr, "interface name is too long: %s\n", argv[1]);
		return 2;
	}
	errno = 0;
	requested = strtoul(argv[2], &end, 10);
	if (errno || !end || *end || requested == 0 || requested > UINT32_MAX) {
		fprintf(stderr, "invalid combined queue count: %s\n", argv[2]);
		return 2;
	}

	fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
	if (fd < 0) {
		perror("socket");
		return 1;
	}
	strcpy(request.ifr_name, argv[1]);
	if (get_channels(fd, &request, &channels) != 0) {
		perror("ETHTOOL_GCHANNELS");
		close(fd);
		return 1;
	}
	printf("zc-net-channels-before: interface=%s max_combined=%u combined=%u "
	       "max_rx=%u rx=%u max_tx=%u tx=%u\n",
	       request.ifr_name, channels.max_combined, channels.combined_count,
	       channels.max_rx, channels.rx_count, channels.max_tx,
	       channels.tx_count);
	if (requested > channels.max_combined) {
		fprintf(stderr, "requested %lu combined queues, maximum is %u\n",
			requested, channels.max_combined);
		close(fd);
		return 1;
	}

	channels.cmd = ETHTOOL_SCHANNELS;
	channels.rx_count = 0;
	channels.tx_count = 0;
	channels.other_count = 0;
	channels.combined_count = (unsigned int)requested;
	request.ifr_data = (void *)&channels;
	if (ioctl(fd, SIOCETHTOOL, &request) != 0) {
		perror("ETHTOOL_SCHANNELS");
		close(fd);
		return 1;
	}
	if (get_channels(fd, &request, &channels) != 0) {
		perror("ETHTOOL_GCHANNELS after set");
		close(fd);
		return 1;
	}
	printf("zc-net-channels-after: interface=%s combined=%u\n",
	       request.ifr_name, channels.combined_count);
	close(fd);
	return channels.combined_count == requested ? 0 : 1;
}
