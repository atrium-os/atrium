"""Score candidate per-jail syscall filter profiles against past advisories.

usage: score-profiles.py <advisories.csv> <gates.csv> [matrix-out.csv]
                         [--policy etc/jaild.policy.toml]

With --policy, the shipped [sysfil] profiles are scored as well, from the
lists the kernel would actually get.

advisories.csv is the review's classified FreeBSD-SA list. gates.csv gives,
for every advisory an ordinary jailed app could reach (atrium_exposure A),
what a filter would have to deny to make it unreachable:
  requirements  ';'-separated; ALL are needed to reach the bug, so denying
                any one of them closes it
  alternatives  '|'-separated inside a requirement; ANY of them reaches it,
                so all must be denied
  atoms         sys:<syscall> ioctl:<cmd> sockopt:<name> af:<family>
                largepage (shm_open2 SHM_LARGEPAGE) core (core dumps)
  none          nothing a v1 filter can deny
Denominators are kernel advisories only, as in the review (44 A rows, 26 of
them memory-safety bugs); the two CPU-errata rows and libnv are never closed.
"""
import csv,sys
args=sys.argv[1:]
policy=None
if '--policy' in args:
    i=args.index('--policy'); policy=args[i+1]; del args[i:i+2]
sys.argv=[sys.argv[0]]+args
data=sys.argv[1]; gates=sys.argv[2]
rows={r['advisory']:r for r in csv.DictReader(open(data,newline=''))}
G={r['advisory']:r['requirements'] for r in csv.DictReader(open(gates,newline=''))}
A=[a for a,r in rows.items() if r['atrium_exposure'].strip()=='A']
assert set(A)==set(G),(set(A)^set(G))
A=[a for a in A if rows[a]['where']=='kernel']
MEM={'spatial','temporal','uninit'}
SOCKOPT_ALL={'IPV6_MSFILTER','TCP_RXTLS_ENABLE','SO_REUSEPORT_LB','SO_ACCEPTFILTER','IPV6_2292PKTOPTIONS'}
base_denied_sys={'kqueuex','ktrace','ktimer_settime','jail_set','jail_get','procctl','sigqueue','sendfile','thr_kill2','ptrace','setcred','copy_file_range','lio_listio','execve','fexecve'}
def prof(sys_allow=(),ioctl=True,sockopt=True,af=True,largepage=True,core=False,sysfilter=True,ioctl_allow=(),sockopt_allow=(),af_allow=()):
    return dict(sys_allow=set(sys_allow),sysfilter=sysfilter,ioctl=ioctl,sockopt=sockopt,af=af,largepage=largepage,core=core,ioctl_allow=set(ioctl_allow),sockopt_allow=set(sockopt_allow),af_allow=set(af_allow))
P=[
 ('none (today)', prof(sysfilter=False,ioctl=False,sockopt=False,af=False,largepage=False)),
 ('syscall bitmap only, no argument filters', prof(ioctl=False,sockopt=False,af=False,largepage=False)),
 # jaild's launcher runs execve inside the filtered jail, so every class
 # allows execve(2) (not fexecve); see jail-sysfil.md §3.6.
 ('renderer / decoder', prof(sys_allow={'execve'})),
 ('document worker', prof(sys_allow={'execve'})),
 ('GUI app (default)', prof(sys_allow={'execve'})),
 ('networked app', prof(sys_allow={'execve'},af_allow={'inet','inet6'})),
 ('app classes if the filter started at exec (v2)', prof()),
 ('terminal', prof(sys_allow={'execve','fexecve','copy_file_range','ktimer_settime'},ioctl_allow={'TIOCSCTTY','TIOCNOTTY'},af_allow={'inet','inet6'})),
]
# Numeric forms of the atoms, for scoring the shipped profiles.
IOCTL_NUM={'FIOSSHMLPGCNF':0x80306664,'TIOCSCTTY':0x20007461,'TIOCNOTTY':0x20007471}
SOCKOPT_NUM={'IPV6_MSFILTER':(41,74),'TCP_RXTLS_ENABLE':(6,41),'SO_REUSEPORT_LB':(0xffff,0x10000),
             'SO_ACCEPTFILTER':(0xffff,0x1000),'IPV6_2292PKTOPTIONS':(41,25)}
def from_policy(path):
    import tomllib
    prof=tomllib.load(open(path,'rb'))['sysfil']['profiles']
    def fold(n,acc):
        for inc in prof[n].get('include',[]): fold(inc,acc)
        for k in ('syscalls','ioctls','sockopts','families'):
            if k in prof[n]: acc.setdefault(k,set()).update(prof[n][k])
        return acc
    out=[]
    for n in prof:
        if n=='base': continue
        a=fold(n,{})
        io={int(x,16) for x in a.get('ioctls',[])}
        so={tuple(int(v,0) for v in x.split(':')) for x in a.get('sockopts',[])}
        p=prof_from_sets(a.get('syscalls'),io if 'ioctls' in a else None,so if 'sockopts' in a else None,a.get('families'))
        out.append((f'shipped profile "{n}"',p))
    return out
def prof_from_sets(sysl,io,so,fam):
    return dict(sys_allow=set(sysl or ()),sysfilter=sysl is not None,
        ioctl=io is not None,ioctl_allow={k for k,v in IOCTL_NUM.items() if io and v in io}|({'SIOCSIFxxx'} if io and any((x>>8)&0xff==ord('i') for x in io) else set()),
        sockopt=so is not None,sockopt_allow={k for k,v in SOCKOPT_NUM.items() if so and v in so},
        af=fam is not None,af_allow=set(fam or ()),largepage=io is not None and IOCTL_NUM['FIOSSHMLPGCNF'] not in io,core=False)
if policy: P+=from_policy(policy)
def denied(atom,p):
    k,_,v=atom.partition(':')
    if k=='sys': return p['sysfilter'] and v not in p['sys_allow']
    if k=='ioctl': return p['ioctl'] and v not in p['ioctl_allow']
    if k=='sockopt': return p['sockopt'] and v not in p['sockopt_allow']
    if k=='af': return p['af'] and v not in p['af_allow']
    if k=='largepage': return p['largepage']
    if k=='core': return p['core']
    raise Exception(atom)
def closed(adv,p):
    req=G[adv]
    if req=='none': return False
    return any(all(denied(a,p) for a in r.split('|')) for r in req.split(';'))
mem=[a for a in A if rows[a]['bug_class'] in MEM]
print('kernel A rows',len(A),'memory-safety',len(mem))
print('| profile | closed | memory-safety closed |')
print('|---|---|---|')
out=[]
for name,p in P:
  for core in (False,True):
    p2=dict(p,core=core)
    c=[a for a in A if closed(a,p2)]
    cm=[a for a in c if a in mem]
    out.append((name,core,len(c),len(cm),c))
    print(f"| {name} {'+ no core dumps' if core else ''} | {len(c)}/{len(A)} | {len(cm)}/{len(mem)} |")
# per-advisory matrix
if len(sys.argv)>3:
  w=csv.writer(open(sys.argv[3],'w',newline=''))
  w.writerow(['advisory','bug_class','requirements','note']+[n for n,_ in P])
  notes={r['advisory']:r['entry_point_note'] for r in csv.DictReader(open(gates,newline=''))}
  for a in sorted(A,reverse=True):
    w.writerow([a,rows[a]['bug_class'],G[a],notes[a]]+['closed' if closed(a,dict(p,core=True)) else '' for n,p in P])
