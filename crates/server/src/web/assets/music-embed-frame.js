// Only public geometry crosses the frame boundary; playback and account data
// stay inside the iframe. The publisher validates both source window and origin.
(()=>{
  if(parent===window)return;
  const content=document.querySelector('main.focused-main');
  if(!content)return;
  let last=0;
  const report=()=>{
    const height=Math.min(10000,Math.max(100,Math.ceil(content.getBoundingClientRect().height)));
    if(height===last)return;
    last=height;
    parent.postMessage({source:'plamenu-music-embed',type:'resize',height},'*');
  };
  if(typeof ResizeObserver!=='undefined')new ResizeObserver(report).observe(content);
  window.addEventListener('load',report,{once:true});
  report();
})();
