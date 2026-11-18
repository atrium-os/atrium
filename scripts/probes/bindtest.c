/* A plain bind on 127.0.0.1:<port> (vm-registries-check.sh): ok, or why not. */
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
int main(int c, char **v) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_len = sizeof a; a.sin_family = AF_INET; a.sin_port = htons(atoi(v[1]));
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(s, (struct sockaddr *)&a, sizeof a) != 0) { printf("bind %s: %s\n", v[1], strerror(errno)); return 1; }
    printf("bind %s: ok\n", v[1]); return 0;
}
