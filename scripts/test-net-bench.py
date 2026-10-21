#!/usr/bin/env python3
"""Exercise bounded pipeline credit, correlation, and fail-closed topology checks."""
import json,os,pathlib,socket,struct,subprocess,sys,threading
binary=str(pathlib.Path(sys.argv[1]).resolve())
if not pathlib.Path(binary).is_file():raise SystemExit('probe binary does not exist: '+binary)
def exact(s,n):
 data=b''
 while len(data)<n:
  part=s.recv(n-len(data))
  if not part:raise EOFError()
  data+=part
 return data
def exercise(corrupt=False, durable=False):
 with socket.socket() as listener:
  listener.bind(('127.0.0.1',0));listener.listen();listener.settimeout(3);address='127.0.0.1:'+str(listener.getsockname()[1]);errors=[]
  def serve():
   try:
    with listener.accept()[0] as stream:
     stream.settimeout(3)
     for window in range(3):
      frames=[]
      for _ in range(4):
       h=bytearray(exact(stream,64));assert struct.unpack_from('<H',h,12)[0]==1
       assert len(exact(stream,struct.unpack_from('<I',h,20)[0]))==4096
       sync=None
       if durable:
        sync=bytearray(exact(stream,64));assert struct.unpack_from('<H',sync,12)[0]==7
       frames.append((h,sync))
      # No response credit has been returned. A fifth request would violate QD4.
      stream.settimeout(.03)
      try: assert not stream.recv(1,socket.MSG_PEEK), 'pipeline exceeded outstanding limit'
      except socket.timeout:pass
      stream.settimeout(3)
      for h,sync in frames:
       struct.pack_into('<H',h,12,4)
       if corrupt and not durable:
        struct.pack_into('<Q',h,48,999);stream.sendall(h);return
       stream.sendall(h)
       if sync is not None:
        struct.pack_into('<H',sync,12,8)
        if corrupt:struct.pack_into('<Q',sync,48,999)
        stream.sendall(sync)
        if corrupt:return
     h=bytearray(exact(stream,64));assert struct.unpack_from('<H',h,12)[0]==7
     struct.pack_into('<H',h,12,8);stream.sendall(h)
   except BaseException as e:errors.append(e)
  thread=threading.Thread(target=serve);thread.start()
  cmd=[binary,'run',address,'write-sync' if durable else 'write','12','1','4','4096','262144','1','sequential','wal-durable-sync' if durable else 'volatile-ram']
  if durable:cmd.append('100')
  result=subprocess.run(cmd,capture_output=True,text=True,timeout=5)
  thread.join(timeout=4);assert not thread.is_alive();assert not errors,errors
  if corrupt:assert result.returncode!=0 and 'mismatch' in result.stderr,result
  else:
   assert result.returncode==0,result.stderr
   record=json.loads(result.stdout);assert record['aggregate_outstanding_limit']==4 and record['operations']==12
   assert record['aggregate_wire_request_limit']==(8 if durable else 4)
   if durable:
    assert record['sync_included_in_iops'] and not record['final_sync_included_in_iops']
    assert record['completion_semantics']=='remote_write_plus_durable_sync_ack'
    assert record['completion_reference_us']==100 and record['optimistic_sync_reference_iops_ceiling']==40000
    assert record['rtt_iops_ceiling'] is None and record['actual_theoretical_efficiency'] is None and record['durable_ceiling_unavailable']
exercise();exercise(corrupt=True);exercise(durable=True);exercise(corrupt=True,durable=True)
env=os.environ.copy();env['URING_PLAY_TOPOLOGY_STRICT']='1'
cmd=[binary,'run','127.0.0.1:1','read','1','1','1','4096','4096','1','random','volatile-ram']
r=subprocess.run(cmd,env=env,capture_output=True,text=True,timeout=3)
assert r.returncode!=0 and not r.stdout and 'strict topology' in r.stderr
# A failed connection with multiple requested workers must not strand a barrier.
cmd[6]='4'
r=subprocess.run(cmd,capture_output=True,text=True,timeout=3)
assert r.returncode!=0 and not r.stdout
cmd=[binary,'run','127.0.0.1:1','write','1','1','1','4096','4096','1','random','wal-durable-sync']
r=subprocess.run(cmd,capture_output=True,text=True,timeout=3)
assert r.returncode!=0 and not r.stdout and 'invalid' in r.stderr
print('PASS: bounded QD, WRITE/SYNC correlation, completion semantics, strict preflight, and connection failure')
