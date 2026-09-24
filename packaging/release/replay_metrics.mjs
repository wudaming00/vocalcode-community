// Metrics for the synthetic-only QA runners. Punctuation and case are ignored;
// these are not semantic correctness or speaker/accent quality measurements.
export function units(text,language){
  const value=text.normalize('NFKC').toLowerCase().replaceAll('’',"'");
  return language==='zh'?[...value].filter(c=>/[\p{L}\p{N}]/u.test(c)):value.replaceAll("'",'').match(/[\p{L}\p{N}]+/gu)??[];
}
export function distance(reference,hypothesis){
  let row=Array.from({length:hypothesis.length+1},(_,i)=>i);
  for(let i=0;i<reference.length;i++){
    const next=[i+1];
    for(let j=0;j<hypothesis.length;j++)next.push(Math.min(row[j+1]+1,next[j]+1,row[j]+Number(reference[i]!==hypothesis[j])));
    row=next;
  }
  return row[hypothesis.length];
}
export function changes(reference,hypothesis){
  if(reference.length>2500||hypothesis.length>2500)throw Error('Diff is limited to 2500 normalized units per side');
  const width=hypothesis.length+1,table=new Uint16Array((reference.length+1)*width);
  for(let i=0;i<=reference.length;i++)table[i*width]=i;
  for(let j=0;j<=hypothesis.length;j++)table[j]=j;
  for(let i=1;i<=reference.length;i++)for(let j=1;j<=hypothesis.length;j++)table[i*width+j]=Math.min(table[(i-1)*width+j]+1,table[i*width+j-1]+1,table[(i-1)*width+j-1]+Number(reference[i-1]!==hypothesis[j-1]));
  let i=reference.length,j=hypothesis.length;const result=[];
  while(i||j){
    const here=table[i*width+j];
    if(i&&j&&here===table[(i-1)*width+j-1]+Number(reference[i-1]!==hypothesis[j-1])){
      if(reference[i-1]!==hypothesis[j-1])result.push({kind:'substitute',reference:reference[i-1],hypothesis:hypothesis[j-1],at:i-1});
      i--;j--;
    }else if(i&&here===table[(i-1)*width+j]+1){result.push({kind:'delete',reference:reference[i-1],hypothesis:'',at:i-1});i--;}
    else{result.push({kind:'insert',reference:'',hypothesis:hypothesis[j-1],at:i});j--;}
  }
  return result.reverse();
}
