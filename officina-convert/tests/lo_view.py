# Opens a file in LibreOffice with a view (under Xvfb) and saves it in another
# format: headless conversion has no view, so view settings such as frozen
# panes are lost on the way. Used by tests/freeze.rs.
# Usage: lo_view.py SRC DST FILTER PROFILE_DIR PORT
import subprocess, sys, time

import uno
from com.sun.star.beans import PropertyValue


def pv(n, v):
    p = PropertyValue()
    p.Name, p.Value = n, v
    return p


src, dst, flt, prof, port = sys.argv[1:6]
conn = f"socket,host=127.0.0.1,port={port};urp;"
proc = subprocess.Popen(["soffice", "-env:UserInstallation=file://" + prof, "--norestore", "--nologo", "--accept=" + conn])
ctx = None
for _ in range(60):
    try:
        local = uno.getComponentContext()
        res = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
        ctx = res.resolve("uno:" + conn + "StarOffice.ComponentContext")
        break
    except Exception:
        time.sleep(1)
desk = ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", ctx)
doc = desk.loadComponentFromURL(uno.systemPathToFileUrl(src), "_blank", 0, (pv("Hidden", False),))
time.sleep(2)
doc.storeToURL(uno.systemPathToFileUrl(dst), (pv("FilterName", flt),))
doc.close(True)
try:
    desk.terminate()
except Exception:
    pass
proc.wait(timeout=30)
