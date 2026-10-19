// Fixture-only WebSocket snapshots exercise display edge cases without changing the library.
export async function listeningRoomFlows({js, send, check, waitUntil, tap, screenshot}) {
  const injected = await send('Page.addScriptToEvaluateOnNewDocument', {source: `
    const NativeSocket = window.WebSocket;
    window.WebSocket = class extends NativeSocket {
      constructor(...args) {
        super(...args);
        if (/\\/api\\/(players|zones)\\/[^/]+\\/ws$/.test(String(args[0]))) {
          window.roomSocket = this;
          this.addEventListener('message', e => {
            const state = JSON.parse(e.data);
            if (state.type !== 'progress') window.roomState = state;
          });
        }
      }
    };
    window.roomSamples = 0;
    const sample = CanvasRenderingContext2D.prototype.getImageData;
    CanvasRenderingContext2D.prototype.getImageData = function(...args) { window.roomSamples++; return sample.apply(this,args); };
  `});
  await send('Emulation.setDeviceMetricsOverride',{width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await js(`history.replaceState(null,'','/');localStorage.setItem('musicata.browse-view','library')`);
  await send('Page.reload');
  check('Listening room: fixture receives real playback state',await waitUntil('window.roomState && document.querySelector(".album-card")',5000)<Infinity);
  await tap('.now-title-button');
  await js(`window.roomOriginal=structuredClone(window.roomState);
    window.roomShow=(art,title='A long recording — '+ 'UnbrokenTitle'.repeat(25))=>{
      const state={...window.roomOriginal,status:'paused',elapsed_seconds:0,duration_seconds:240,queue:[],queue_position:null,
        now_playing:{...window.roomOriginal.now_playing,track_id:null,title,artist:'An artist with a very long name '.repeat(8),album:'An album with a very long title '.repeat(8),artwork_url:art}};
      window.roomSocket.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(state)}));
    };window.roomShow(null)`);
  await waitUntil('document.querySelector(".artwork-unavailable")',2000);
  check('Listening room: missing art and empty queue have explicit fallbacks',await js(`!!document.querySelector('.artwork-unavailable') && document.querySelector('.queue-empty')?.textContent.includes('empty')`));
  for (const [width,height] of [[1440,900],[390,844],[320,568],[844,390]]) {
    await send('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:width<1024});
    await new Promise(r=>setTimeout(r,150));
    check('Listening room '+width+': long metadata does not displace queue controls',await js(`(()=>{const e=document.querySelector('.playback-modes button');const r=e.getBoundingClientRect();return e.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2)) && document.documentElement.scrollWidth<=innerWidth})()`));
    await screenshot('listening-'+width+'-missing-art');
  }
  await send('Emulation.setDeviceMetricsOverride',{width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await js(`window.roomShow('/api/artwork/nonexistent-listening-test')`);
  check('Listening room: failed art load reveals monogram',await waitUntil('document.querySelector(".artwork-unavailable") && !document.querySelector(".playing-cover img")',2000)<Infinity);
  // The synthetic audio library has no covers; use the bundled same-origin image.
  const cover='/icon.svg';
  if(cover) {
    await js(`window.roomSamples=0;window.roomShow(${JSON.stringify(cover)},'Listening room')`);
    check('Listening room: new track recovers from failed artwork',await waitUntil('document.querySelector(".playing-cover img")?.naturalWidth>0 && !document.querySelector(".artwork-unavailable")',3000)<Infinity);
    check('Listening room: cover is sampled once',await js('window.roomSamples===1'));
    await js(`window.roomMutations=0;window.roomObserver=new MutationObserver(()=>window.roomMutations++);
      for(const e of document.querySelectorAll('.listening-title,.playing-cover'))window.roomObserver.observe(e,{subtree:true,childList:true,characterData:true,attributes:true});
      window.roomColor=document.querySelector('.listening-room').style.getPropertyValue('--listening-bg');
      for(let elapsed=1;elapsed<=120;elapsed++)window.roomSocket.dispatchEvent(new MessageEvent('message',{data:JSON.stringify({type:'progress',elapsed_seconds:elapsed,duration_seconds:240})}));`);
    await new Promise(r=>setTimeout(r,150));
    check('Listening room: progress leaves artwork, heading and background unchanged',await js(`window.roomSamples===1 && window.roomMutations===0 && document.querySelector('.listening-room').style.getPropertyValue('--listening-bg')===window.roomColor`));
    await screenshot('listening-desktop-artwork');
    await js('window.roomObserver.disconnect()');
  }
  await send('Page.removeScriptToEvaluateOnNewDocument',{identifier:injected.identifier});
  await send('Page.reload');
  await waitUntil('document.querySelector(".transport")',5000);
}
