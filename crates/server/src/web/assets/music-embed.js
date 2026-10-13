// Public placeholders become isolated, read-only frames. No HTML or credentials
// from the embedding page are sent to Plamenu.
(()=>{
  const source=document.currentScript?.src;
  if(!source)return;
  const origin=new URL(source).origin;
  const frames=new Set();
  window.addEventListener('message',event=>{
    if(event.origin!==origin||event.data?.source!=='plamenu-music-embed'||event.data.type!=='resize'||!Number.isFinite(event.data.height))return;
    for(const frame of frames){
      if(event.source===frame.contentWindow)frame.height=String(Math.min(10000,Math.max(100,Math.ceil(event.data.height))));
    }
  });
  for(const node of document.querySelectorAll('[data-plamenu-music]')){
    const kind=node.dataset.plamenuMusic;
    const id=node.dataset.id;
    if(node.dataset.plamenuMusicLoaded||!['assets','artists','albums','collections'].includes(kind)||!/^\d{1,20}$/.test(id??''))continue;
    const frame=document.createElement('iframe');
    const url=new URL(`/music/embed/${kind}/${id}`,origin);
    if(node.dataset.codec){
      const bitrate=Number(node.dataset.bitrate);
      if(!['mp3','ogg','flac','aac','opus'].includes(node.dataset.codec)||!Number.isInteger(bitrate)||bitrate<32||bitrate>512)continue;
      url.searchParams.set('codec',node.dataset.codec);url.searchParams.set('bitrate',String(bitrate));
    }
    frame.src=url.href;
    frame.title=node.dataset.title||'Plamenu music';
    frame.width='100%';frame.height=kind==='assets'?'280':'500';
    frame.loading='lazy';frame.referrerPolicy='no-referrer';frame.allow='autoplay';
    node.dataset.plamenuMusicLoaded='true';node.append(frame);frames.add(frame);
  }
})();
