from pathlib import Path
import numpy as np, onnxruntime as ort, wave
root=Path(__file__).resolve().parent
source=root.parents[3]
x=np.frombuffer((root/'input.pcm16').read_bytes(),dtype='<i2').astype(np.float32)/32768
s=ort.InferenceSession(str(source/'vendor/models/gtcrn/gtcrn.onnx'),providers=['CPUExecutionProvider'])
print([(i.name,i.shape) for i in s.get_inputs()])
def kernel(cutoff,phase=0):
 distance=np.arange(64)-32+phase;z=distance/32
 window=np.where(abs(z)<1,.42+.5*np.cos(np.pi*z)+.08*np.cos(2*np.pi*z),0)
 h=window*cutoff*np.sinc(cutoff*distance);return (h/h.sum()).astype(np.float32)
down=np.convolve(x,kernel(1/3))[:len(x)][2::3]
window=np.sqrt(.5*(1-np.cos(2*np.pi*np.arange(512)/512))).astype(np.float32)
state=[np.zeros(i.shape,np.float32) for i in s.get_inputs()[1:]]
history=np.zeros(256,np.float32);overlap=history.copy();out=[np.zeros(256,np.float32)]
for start in range(0,len(down)-255,256):
 block=down[start:start+256];time=np.concatenate([history,block])*window;history=block
 spec=np.fft.rfft(time);packed=np.stack([spec.real,spec.imag],axis=-1).astype(np.float32).reshape(1,257,1,2)
 result=s.run(None,dict(zip([i.name for i in s.get_inputs()],[packed,*state])));state=result[1:]
 bins=result[0].reshape(257,2);enh=np.fft.irfft(bins[:,0]+1j*bins[:,1],512).astype(np.float32)*window
 out.append(overlap+enh[:256]);overlap=enh[256:]
output=np.concatenate(out)[:len(down)]
up=np.zeros((len(output),3),np.float32)
for phase in range(3):
 pos=phase/3*1024;i=np.floor(pos);blend=pos-i
 h=kernel(1,i/1024)*(1-blend)+kernel(1,(i+1)/1024)*blend
 up[:,phase]=np.convolve(output,h)[:len(output)]
up.ravel().astype('<f4').tofile(root/'gtcrn.f32')
print('GTCRN independent ONNX Runtime reference:',up.size,'samples, energy',float((up*up).sum()))
