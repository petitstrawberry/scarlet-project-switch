#!/usr/bin/env python3
"""Run exact NCM encoding/parsing and xHCI ownership paths with modeled DMA.

No remote access; isolated pinned checkouts are removed after preserving evidence.
"""
import argparse, hashlib, json, re, runpy, shutil, subprocess, tempfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
prior=runpy.run_path(str(ROOT/'tests/test-network-packet-allocation.py'))
block=prior['block']
PATCHES=prior['PATCHES']+prior['NEW']+['ncm-dma-batching.patch']
def sha(p): return hashlib.sha256(p.read_bytes()).hexdigest()
def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,default=ROOT/'.cache/network-perf/ncm-tx-batch/validation');a=p.parse_args();out=a.output.resolve();out.mkdir(parents=True,exist_ok=True)
 core=ROOT/'projects/aarch64-switch-l4t-console/.scarlet/cache/cargo-home/git/checkouts/scarlet-26bf9663864ed506/6fa4a4a'
 paths=['kernel/src/drivers/usb/cdc_ncm.rs','kernel/src/drivers/usb/xhci/mod.rs']
 before={n:sha(core/n) for n in paths}
 receipt={'patches':{n:sha(ROOT/'patches/scarlet'/n) for n in PATCHES},'runs':[],'physical_tested':False}
 with tempfile.TemporaryDirectory(prefix='validation-',dir=out) as td:
  td=Path(td);c=td/'core';subprocess.run(['git','clone','--quiet','--local','--no-hardlinks',str(core),str(c)],check=True);subprocess.run(['git','checkout','--quiet','--detach',prior['BASE']],cwd=c,check=True)
  for n in PATCHES: subprocess.run(['git','apply',str(ROOT/'patches/scarlet'/n)],cwd=c,check=True)
  subprocess.run(['git','diff','--check'],cwd=c,check=True)
  ns=(c/paths[0]).read_text();xs=(c/paths[1]).read_text()
  constants='\n'.join(re.findall(r'^(?:pub\(crate\) )?const (?:NTH16_\w+|NDP16_\w+|NCM_\w+|ETHERNET_HEADER_LENGTH):.*;$',ns,re.M))
  packet=(c/'kernel/src/device/network/mod.rs').read_text()
  types=block(packet,'pub struct DevicePacket {')+'\n'+block(packet,'impl DevicePacket {')
  ncm=constants+'\n'+types+'\n#[derive(Clone, Copy)]\n'+block(ns,'pub(crate) struct CdcNcmParameters {')+'\n'+block(ns,'impl CdcNcmParameters {')+'\n#[derive(Clone, Copy)]\n'+block(ns,'struct Ntb16TxConfig {')+'\n'+block(ns,'impl Ntb16TxConfig {')
  for marker in ['fn sanitize_alignment(', 'fn align_to_remainder(', 'pub(crate) struct Ntb16TxBatch {','impl Ntb16TxBatch {','fn build_ntb16(', 'fn parse_ntb16(','fn read_u16(','fn read_u32(','fn write_u16(','fn write_u32(']:ncm+='\n'+block(ns,marker)
  rxmethod=block(xs,'    fn handle_transfer_event(');prefix=rxmethod[:rxmethod.index('        if let Some(ncm)')];rx=block(rxmethod,'        if let Some(ncm) = slot.cdc_ncm.as_mut()\n            && ncm.bulk_in.dci == endpoint_id')
  tx=block(rxmethod,'        if let Some(ncm) = slot.cdc_ncm.as_mut()\n            && ncm.bulk_out.dci == endpoint_id')
  ring=(c/'kernel/src/drivers/usb/xhci/ring.rs').read_text();trb=(c/'kernel/src/drivers/usb/xhci/trb.rs').read_text()
  controller=trb[:trb.index('#[cfg(test)]')]+'\n'
  for marker in ['struct CdcNcmDmaBuffer {','struct InFlightCdcNcmRx {','struct CompletedCdcNcmRx {','struct InFlightCdcNcmTx {','fn sync_pages_before_device_write(','fn sync_pages_after_device_write(']:controller+=block(xs,marker)+'\n'
  controller+=block(ring,'pub struct DmaTrbRing {')+'\n'+block(ring,'impl DmaTrbRing {')+'\nimpl XhciController {\n'+prefix+tx+rx+'\nfalse\n}\n'
  for marker in ['    fn transfer_successful(','    fn complete_cdc_ncm_rx(','    fn enqueue_cdc_ncm_tx(','    fn take_pending_cdc_ncm_tx(','    fn restore_cdc_ncm_tx_buffer(','    fn process_pending_cdc_ncm_tx(']:controller+=block(xs,marker)+'\n'
  controller+='}\n'
  queue='\n'.join(block(ns,m) for m in ['struct QueuedRxPacket {','fn enqueue_rx_packets(','fn drain_queued_rx_packets('])
  tests=[('ncm',ncm,'/* PRODUCTION_NCM */','ncm-dma-host-tests.rs'),('ownership',ncm+'\n'+controller,'/* PRODUCTION_PATHS */','ncm-dma-ownership-host-tests.rs')]
  rustc=shutil.which('rustc');host=re.search(r'^host: (.+)$',subprocess.check_output([rustc,'-vV'],text=True),re.M)[1]
  for name,prod,marker,h in tests:
   harness=ROOT/'tests/usb-probe-qa'/h;source=harness.read_text().replace(marker,prod).replace('/* PRODUCTION_RX_QUEUE */',queue);rs=out/(name+'.rs');rs.write_text(source)
   compile=subprocess.run([rustc,'--edition=2024','--target',host,'-C','opt-level=2','--test',str(rs),'-o',str(td/name)],capture_output=True,text=True)
   (out/(name+'-compile.log')).write_text(compile.stdout+compile.stderr);compile.check_returncode()
   result=subprocess.run([str(td/name),'--test-threads=1','--nocapture'],capture_output=True,text=True,timeout=30);(out/(name+'-tests.log')).write_text(result.stdout+result.stderr);result.check_returncode()
   print(result.stdout);receipt['runs'].append({'name':name,'harness_sha256':sha(harness),'production_sha256':hashlib.sha256(prod.encode()).hexdigest(),'result':result.stdout.split('test result: ')[-1].strip()})
  receipt['source_sha256']={n:sha(c/n) for n in paths}
 assert before=={n:sha(core/n) for n in paths}
 receipt.update(result='pass',temporary_checkout_removed=True,cache_sources_unchanged=True)
 (out/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
if __name__=='__main__':main()
