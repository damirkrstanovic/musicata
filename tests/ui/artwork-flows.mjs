// Album artwork is an administrator-only, inline editor. Keep this separate from the broad
// player smoke so its upload and responsive regressions stay legible.
export async function artworkFlows({js, send, check, waitUntil, screenshot}) {
  await js(`document.querySelector('.album-card .card-text')?.click()`);
  check('Artwork: opens an album detail', await waitUntil(`document.querySelector('.detail-hero')`, 2000) < Infinity);
  await js(`document.querySelector('.output-trigger')?.click()`);
  await js(`document.querySelector('[data-output="player:browser-local"]')?.click()`);
  await js(`document.querySelector('.detail-hero .primary-button')?.click()`);
  check('Artwork: starts playback before editing', await waitUntil(`document.querySelector('audio') && !document.querySelector('audio').paused && document.querySelector('audio').currentTime>0`,3000)<Infinity);
  await js(`window.__artworkAudio=document.querySelector('audio')`);
  const playback = await js(`({time:window.__artworkAudio.currentTime,src:window.__artworkAudio.currentSrc})`);


  await js(`document.querySelector('.artwork-disclosure')?.click()`);
  check('Artwork: Change artwork loads its review', await waitUntil(`document.querySelector('.artwork-workbench') && !document.querySelector('.artwork-workbench .admin-hint')?.textContent.includes('Loading')`, 2000) < Infinity);
  check('Artwork: review is visible to the signed-in admin', await js(`!!document.querySelector('.artwork-workbench')`));

  // A browser-produced PNG exercises the exact raw-file upload path used by the label/input.
  await js(`(()=>{const input=document.querySelector('.artwork-upload input');const canvas=document.createElement('canvas');canvas.width=2;canvas.height=2;const ctx=canvas.getContext('2d');ctx.fillStyle='#b58d4f';ctx.fillRect(0,0,2,2);canvas.toBlob(blob=>{const data=new DataTransfer();data.items.add(new File([blob],'smoke-art.png',{type:'image/png'}));input.files=data.files;input.dispatchEvent(new Event('change',{bubbles:true}));},'image/png')})()`);
  check('Artwork: uploads a valid image', await waitUntil(`document.querySelector('.artwork-status')?.textContent.includes('uploaded')`, 3000) < Infinity);
  check('Artwork: manual choice enables Clear', await js(`!document.querySelector('.artwork-actions .danger')?.disabled`));
  check('Artwork: uploaded cover renders', await waitUntil(`document.querySelector('.hero-cover img')?.naturalWidth>0`,2000)<Infinity);
  await screenshot('artwork-editor-desktop');

  await js(`document.querySelector('.artwork-workbench header .ghost-button')?.click()`);
  check('Artwork: refresh preserves the manual choice', await waitUntil(`document.querySelector('.artwork-status')?.textContent.includes('refreshed')`, 3000) < Infinity && await js(`!document.querySelector('.artwork-actions .danger')?.disabled`));

  await js(`(()=>{const input=document.querySelector('.artwork-upload input');const data=new DataTransfer();data.items.add(new File(['nope'],'not-art.txt',{type:'text/plain'}));input.files=data.files;input.dispatchEvent(new Event('change',{bubbles:true}))})()`);
  check('Artwork: invalid upload explains the accepted formats and keeps the choice', await waitUntil(`document.querySelector('.artwork-error')?.textContent.includes('JPEG')`, 1000) < Infinity && await js(`!document.querySelector('.artwork-actions .danger')?.disabled`));

  await send('Emulation.setDeviceMetricsOverride', {width:390,height:844,deviceScaleFactor:1,mobile:true});
  await js(`document.querySelector('.artwork-disclosure')?.focus()`);
  await send('Input.dispatchKeyEvent',{type:'keyDown',key:'Enter',code:'Enter'});
  await send('Input.dispatchKeyEvent',{type:'keyUp',key:'Enter',code:'Enter'});
  await send('Input.dispatchKeyEvent',{type:'keyDown',key:'Enter',code:'Enter'});
  await send('Input.dispatchKeyEvent',{type:'keyUp',key:'Enter',code:'Enter'});
  await waitUntil(`document.querySelector('.artwork-workbench')`, 1000);
  await js(`document.querySelector('.artwork-workbench')?.scrollIntoView({block:'center'})`);
  await screenshot('artwork-editor-390');
  check('Artwork: editor fits a 390px viewport and keyboard disclosure remains usable', await js(`document.documentElement.scrollWidth<=innerWidth && document.activeElement?.classList.contains('artwork-disclosure')`));

  await js(`document.querySelector('.artwork-actions .danger')?.click()`);
  check('Artwork: Clear restores automatic artwork', await waitUntil(`document.querySelector('.artwork-status')?.textContent.includes('Returned to automatic artwork')`, 3000) < Infinity && await js(`document.querySelector('.artwork-actions .danger')?.disabled===true`));
  check('Artwork: editor does not interrupt playback progress', await js(`(()=>{const a=document.querySelector('audio');return a===window.__artworkAudio && !a.paused && (a.currentTime>${playback.time} || a.currentSrc!==${JSON.stringify(playback.src)})})()`));

  await send('Emulation.setDeviceMetricsOverride', {width:1280,height:900,deviceScaleFactor:1,mobile:false});
  await js(`document.querySelector('.back-btn')?.click()`);
  await waitUntil(`document.querySelector('.album-card')`, 2000);
}
