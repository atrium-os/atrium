"""Check the [sysfil] profiles in a jaild policy against a kernel's syscall
table, so a misspelt name fails here instead of at jail creation (the kernel
refuses a filter that names an unknown system call).

usage: check-profiles.py <jaild.policy.toml> <freebsd-src>/sys/kern/syscalls.c
"""
import re, sys, tomllib

prof = tomllib.load(open(sys.argv[1], 'rb'))['sysfil']['profiles']
names = set(re.findall(r'^\s*"([^"]+)",\s*/\* \d+ =', open(sys.argv[2]).read(), re.M))
bad = 0
for pname, p in prof.items():
    for inc in p.get('include', []):
        if inc not in prof:
            print(f'{pname}: includes unknown profile {inc}'); bad += 1
    for s in p.get('syscalls', []):
        if s not in names:
            print(f'{pname}: unknown system call {s}'); bad += 1
    for i in p.get('ioctls', []):
        if not re.fullmatch(r'0x[0-9a-f]+', i):
            print(f'{pname}: ioctl {i} is not lower-case hex'); bad += 1
    for o in p.get('sockopts', []):
        if not re.fullmatch(r'(0x[0-9a-f]+|\d+):\d+', o):
            print(f'{pname}: sockopt {o} is not level:name'); bad += 1
print(f'{len(prof)} profiles, {bad} problems')
sys.exit(1 if bad else 0)
