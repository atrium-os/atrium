/* Reads a whole line: portcullisd writes the request in more than one write.
   A stand-in for cryptad's control socket for the I6.0 run: every
   "sso-attempt name=X" gets one fixed live attempt; everything else an error. */
#include <sys/socket.h>
#include <sys/un.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>
int main(int c,char**v){ if(c!=2)return 2; int s=socket(AF_UNIX,SOCK_STREAM,0); struct sockaddr_un a={0}; a.sun_family=AF_UNIX;
 strncpy(a.sun_path,v[1],sizeof a.sun_path-1); unlink(v[1]); if(bind(s,(void*)&a,sizeof a)||listen(s,16))return 3; chmod(v[1],0666);
 for(;;){int f=accept(s,0,0); if(f<0)continue; char b[512]; ssize_t n=0,k; while(n<(ssize_t)sizeof b-1&&(k=read(f,b+n,sizeof b-1-n))>0){n+=k; if(memchr(b,10,n))break;} if(n>0){b[n]=0;
  const char*r=strncmp(b,"sso-attempt",11)==0?"sso-attempt id=00000000000000000000000000000060\n":"error unsupported\n";
  write(f,r,strlen(r)); fprintf(stderr,"fake-crypta: %.*s -> %s",(int)strcspn(b,"\n"),b,r);} close(f);} }
