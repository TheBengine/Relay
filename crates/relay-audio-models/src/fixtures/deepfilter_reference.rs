use df::tract::{DfParams,DfTract,RuntimeParams};
fn main() {
 let args: Vec<_> = std::env::args().collect();
 let input=std::fs::read(&args[1]).unwrap();
 let mut model=DfTract::new(DfParams::new((&args[2]).into()).unwrap(), &RuntimeParams::default_with_ch(1)).unwrap();
 let mut output=Vec::new();
 for chunk in input.chunks_exact(960) {
  let samples:Vec<f32>=chunk.chunks_exact(2).map(|x|i16::from_le_bytes([x[0],x[1]]) as f32 /32768.0).collect();
  let mut out=[0.0;480];
  model.process(ndarray::ArrayView2::from_shape((1,480),&samples).unwrap(),ndarray::ArrayViewMut2::from_shape((1,480),&mut out).unwrap()).unwrap();
  for x in out {output.extend(x.to_le_bytes());}
 }
 std::fs::write(&args[3],output).unwrap();
}
