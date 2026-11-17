// Test-only LD_PRELOAD for containers whose kernel has no IPv6:
// ippeveprinter insists on an IPv6 listener; give it a throwaway IPv4 one.
// An AF_INET6 lookup returns IPv4 loopback marked in sin_zero; a bind to a
// marked address binds 127.0.0.1 on any free port instead.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <string.h>
static const char mark[8] = "noipv6!";
int getaddrinfo(const char *n, const char *s, const struct addrinfo *h, struct addrinfo **r) {
  static int (*real)(const char*,const char*,const struct addrinfo*,struct addrinfo**);
  if (!real) real = dlsym(RTLD_NEXT, "getaddrinfo");
  if (h && h->ai_family == AF_INET6) {
    struct addrinfo h4 = *h; h4.ai_family = AF_INET; h4.ai_flags &= ~AI_ADDRCONFIG;
    int e = real("127.0.0.1", s, &h4, r);
    if (e == 0) for (struct addrinfo *a = *r; a; a = a->ai_next)
      memcpy(((struct sockaddr_in*)a->ai_addr)->sin_zero, mark, 8);
    return e;
  }
  return real(n, s, h, r);
}
int bind(int fd, const struct sockaddr *a, socklen_t l) {
  static int (*real)(int,const struct sockaddr*,socklen_t); if (!real) real = dlsym(RTLD_NEXT, "bind");
  if (a && a->sa_family == AF_INET && !memcmp(((struct sockaddr_in*)a)->sin_zero, mark, 8)) {
    struct sockaddr_in s; memset(&s,0,sizeof s); s.sin_family=AF_INET; s.sin_addr.s_addr=htonl(0x7f000001);
    return real(fd,(void*)&s,sizeof s);
  }
  return real(fd,a,l);
}
