"""Print a [sysfil] profile from a jaild policy as jail(8) parameters,
resolved the way jaild resolves it (includes folded in, lists sorted and
comma-joined; a list left out is not printed, so it stays unrestricted).

usage: resolve-profile.py <jaild.policy.toml> <class> [mode]
"""
import sys, tomllib
doc = tomllib.load(open(sys.argv[1], 'rb'))
pol = doc['sysfil']
prof = pol['profiles']
def fold(n, acc, depth=0):
    assert depth < 8, 'include depth'
    for inc in prof[n].get('include', []):
        fold(inc, acc, depth + 1)
    for k in ('syscalls', 'ioctls', 'sockopts', 'families'):
        if k in prof[n]:
            acc.setdefault(k, set()).update(prof[n][k])
    return acc
acc = fold(sys.argv[2], {})
# [sysfil_class_modes] may make a class stricter than the policy's mode
# (jaild-policy's Policy::sysfil_class_modes); an explicit mode argument
# wins, for learning.
mode = sys.argv[3] if len(sys.argv) > 3 else str(max(pol.get('mode', 2), doc.get('sysfil_class_modes', {}).get(sys.argv[2], 0)))
out = [f'sysfil.mode={mode}']
for k in ('syscalls', 'ioctls', 'sockopts', 'families'):
    if k in acc:
        out.append(f'sysfil.{k}={",".join(sorted(acc[k]))}')
print(' '.join(out))
