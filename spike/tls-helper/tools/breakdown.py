import re,sys,statistics as st
for f in sys.argv[1:]:
    rows=[list(map(int,m.groups())) for m in (re.search(r'exec->main=(\d+)us certs=(\d+)us config=(\d+)us wait=(\d+)us handshake=(\d+)us maxrss=(\d+)KiB',l) for l in open(f, errors='replace')) if m]
    if not rows: print(f,'none'); continue
    names=['exec->main','certs','config','wait','handshake','maxrssKiB']
    out=[]
    for i,n in enumerate(names):
        c=sorted(r[i] for r in rows); out.append(f"{n} min={c[0]} med={c[len(c)//2]} p90={c[int(len(c)*.9)]} max={c[-1]}")
    print(f, 'n=',len(rows)); print('   '+'\n   '.join(out))
