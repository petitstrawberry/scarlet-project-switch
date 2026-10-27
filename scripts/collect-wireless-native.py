#!/usr/bin/env python3
"""Deploy one prepared native RAM QA bundle and preserve serial evidence."""
import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import select
import subprocess
import termios
import time
import tty

def collect(bundle, console, ctl, timeout):
    output = bundle.parent
    if (output/'serial.log').exists(): raise ValueError('Preserve existing log and prepare a fresh bundle')
    status = subprocess.check_output([ctl,'status'],text=True,timeout=10)
    (output/'status-before.txt').write_text(status)
    if any(line+'\n' not in status for line in ['state=preboot','fallback=disabled','loader=idle']): raise ValueError('Need idle development monitor')
    fd=os.open(console,os.O_RDWR|os.O_NOCTTY|os.O_NONBLOCK); original=termios.tcgetattr(fd)
    captured=bytearray(); result={'hardware_run':True,'pass':False}
    try:
        fcntl.ioctl(fd,termios.TIOCEXCL); tty.setraw(fd)
        # Drain UART during upload/start too. Waiting for `deploy` to return
        # first can lose the last lines when a guest panic stops EL2 exits.
        with (output/'deploy.log').open('w') as deploy_log, (output/'serial.log').open('wb',buffering=0) as log:
            process=subprocess.Popen([ctl,'deploy',str(bundle)],stdout=deploy_log,stderr=subprocess.STDOUT)
            started=time.monotonic(); deadline=started+timeout
            try:
                while time.monotonic()<deadline:
                    code=process.poll()
                    if code is not None and code!=0: raise RuntimeError('RAM deploy failed; inspect deploy.log')
                    if code is None and time.monotonic()-started>45: raise TimeoutError('RAM deploy timed out')
                    if not select.select([fd],[],[],0.2)[0]: continue
                    data=os.read(fd,65536)
                    if not data: break
                    captured.extend(data); log.write(data)
                    # CDC reads can split a marker at any byte. Wait for the
                    # complete line before deciding the run is terminal.
                    complete = captured.rsplit(b'\n', 1)[0] if b'\n' in captured else b''
                    if any(line.startswith((b'WIFI_NATIVE_QA PASS', b'WIFI_NATIVE_QA FAIL',
                                            b'WIFI_CONNECT_QA PASS', b'WIFI_CONNECT_QA FAIL'))
                           for line in complete.splitlines()): break
            finally:
                if process.poll() is None:
                    process.terminate()
                    try: process.wait(timeout=3)
                    except subprocess.TimeoutExpired: process.kill(); process.wait()
        text=captured.decode('utf-8',errors='replace')
        scan_pass='WIFI_NATIVE_QA PASS scan carrier_blocked=true' in text
        scan_fail='WIFI_NATIVE_QA FAIL' in text
        connect_pass='WIFI_CONNECT_QA PASS network=dhcp dns=udp example.com' in text
        connect_fail='WIFI_CONNECT_QA FAIL' in text
        result['scan_pass']=scan_pass
        result['scan_fail']=scan_fail
        result['connection_pass']=connect_pass
        result['connection_fail']=connect_fail
        result['pass']=scan_pass or connect_pass
        result['control_ready']='brcmfmac: ready control=' in text
        result['scan_passes']=text.count('WIFI_NATIVE_QA SCAN pass=')
        result['ap_connection_tested']=connect_pass
        after=subprocess.check_output([ctl,'status'],text=True,timeout=10); (output/'status-after.txt').write_text(after)
    except Exception as error:
        result['error']=str(error); raise
    finally:
        (output/'result.json').write_text(json.dumps(result,indent=2)+'\n')
        try: termios.tcsetattr(fd,termios.TCSANOW,original)
        except OSError as error:
            if error.errno not in (errno.EIO,errno.ENXIO,errno.ENODEV): raise
        finally:
            try: fcntl.ioctl(fd,termios.TIOCNXCL)
            except OSError: pass
            os.close(fd)
    if not result['pass']: raise RuntimeError('Native QA did not pass; inspect serial.log')
    if result.get('connection_pass'):
        print('WIFI_CONNECT_QA PASS WPA2 userspace handshake, DHCP and DNS')
    else:
        print('WIFI_NATIVE_QA PASS physical scan through Native API; carrier blocked')
if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('bundle',type=Path);p.add_argument('--console',default='/dev/cu.usbmodemSWV00011');p.add_argument('--ctl',default='switchvisorctl');p.add_argument('--timeout',type=float,default=100)
    a=p.parse_args()
    if not 0<a.timeout<=600: p.error('timeout must be finite and in (0,600]')
    collect(a.bundle.resolve(),a.console,a.ctl,a.timeout)
