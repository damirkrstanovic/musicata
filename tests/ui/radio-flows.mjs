// Restore the fresh-install radio journey without depending on an external directory.
export async function radioFlows({js, api, check, waitUntil, clickText, screenshot}) {
  for (const station of await api('/api/radio') || []) await api(`/api/radio/${station.id}`, {method:'DELETE'});
  await js(`(() => {
    const fetchOriginal = window.fetch;
    window.__radioDirectoryFail = false;
    window.__radioQueries = [];
    window.__restoreDirectory = () => { window.fetch = fetchOriginal; };
    window.fetch = (input, init) => {
      const url = String(input);
      if (url.includes('/commands')) window.__radioPlaySends = (window.__radioPlaySends || 0) + 1;
      if (window.__radioHoldSave && url === '/api/radio' && init?.method === 'POST') {
        return fetchOriginal(input, init).then(response => new Promise(resolve => {
          window.__radioReleaseSave = () => resolve(response);
        }));
      }
      if (window.__radioPlayFail && url.includes('/commands')) {
        return Promise.resolve(new Response('{}', {status:503, headers:{'content-type':'application/json'}}));
      }
      if (window.__radioAddFail && url === '/api/radio' && init?.method === 'POST') {
        return Promise.resolve(new Response(JSON.stringify({error:{message:'Station save unavailable'}}), {status:503, headers:{'content-type':'application/json'}}));
      }
      if (url.includes('/api/radio/directory')) {
        window.__radioQueries.push(url);
        return Promise.resolve(new Response(JSON.stringify(window.__radioDirectoryFail ? {} : [{
          name:window.__radioNextStation ? 'Deferred FM' : 'Directory Jazz', stream_url:window.__radioNextStation ? 'http://127.0.0.1:1/deferred' : 'http://127.0.0.1:1/jazz', homepage_url:null,
          favicon:'https://outside.invalid/logo.png', country:'Test', tags:'jazz', codec:'MP3', bitrate:128
        }]), {status:window.__radioDirectoryFail ? 503 : 200, headers:{'content-type':'application/json'}}));
      }
      return fetchOriginal(input, init);
    };
  })()`);
  await clickText('.library-panel button', 'Browse radio');
  const opened = await waitUntil(`document.querySelector('.radio-view')`, 3000) < Infinity;
  check('radio discovery reachable with no saved stations', opened);
  if (!opened) { await js(`window.__restoreDirectory()`); return; }
  check('radio opens with popular stations', await waitUntil(`document.querySelector('.directory-station')?.textContent.includes('Directory Jazz')`, 3000) < Infinity);
  check('fresh radio explains saved station empty state', await js(`document.querySelector('.radio-saved')?.textContent.includes('No saved stations')`));
  await js(`(() => {const input=document.querySelector('[aria-label="Search radio stations"]'); input.value='jazz'; input.dispatchEvent(new Event('input',{bubbles:true})); input.form.requestSubmit();})()`);
  check('radio search sends directory query', await waitUntil(`window.__radioQueries.some(q => q.includes('query=jazz'))`, 3000) < Infinity);
  await clickText('.directory-station button', 'Save');
  check('directory station saved through API', await waitUntil(`document.querySelector('.radio-saved')?.textContent.includes('Directory Jazz')`, 3000) < Infinity);
  check('saved stations refresh sidebar', await waitUntil(`[...document.querySelectorAll('.library-panel .nav-link')].some(b=>b.textContent==='Directory Jazz')`, 3000) < Infinity);
  await clickText('.radio-saved button', 'Play');
  check('saved radio plays through relay', await waitUntil(`document.querySelector('#now-title')?.textContent.includes('Directory Jazz')`, 3000) < Infinity);
  await js(`window.__radioPlayFail = true`);
  await clickText('.radio-saved button', 'Play');
  check('radio play failure is visible', await waitUntil(`document.querySelector('.radio-add')?.parentElement.querySelector('[role="alert"]')?.textContent.includes("Couldn't start playback")`, 3000) < Infinity);
  await js(`window.__radioPlayFail = false`);
  await js(`window.__radioDirectoryFail = true; document.querySelector('.radio-search').requestSubmit()`);
  check('directory failure has retry', await waitUntil(`document.querySelector('.radio-directory [role="alert"]')?.textContent.includes('Retry')`, 3000) < Infinity);
  await js(`window.__radioAddFail = true`);
  await js(`(() => {for(const [name,value] of [['name','Custom FM'],['stream_url','http://127.0.0.1:1/custom']]) {const input=document.querySelector('.radio-add [name="'+name+'"]'); input.value=value; input.dispatchEvent(new Event('input',{bubbles:true}));} document.querySelector('.radio-add').requestSubmit();})()`);
  check('custom save failure is visible', await waitUntil(`document.querySelector('.radio-add')?.parentElement.querySelector('[role="alert"]')?.textContent.includes('Station save unavailable')`, 3000) < Infinity);
  check('custom save failure retains form fields', await js(`document.querySelector('.radio-add [name="name"]').value === 'Custom FM' && document.querySelector('.radio-add [name="stream_url"]').value.includes('/custom')`));
  await js(`window.__radioAddFail = false; document.querySelector('.radio-add').requestSubmit()`);
  check('custom radio can be added when directory is unavailable', await waitUntil(`document.querySelector('.radio-saved')?.textContent.includes('Custom FM')`, 3000) < Infinity);
  await js(`[...document.querySelectorAll('.saved-station')].find(row=>row.textContent.includes('Custom FM'))?.querySelector('.danger')?.click()`);
  await waitUntil(`document.querySelector('.modal')`, 1000);
  await clickText('.modal button', 'Remove');
  check('custom station removal updates UI', await waitUntil(`!document.querySelector('.radio-saved')?.textContent.includes('Custom FM')`, 3000) < Infinity);
  check('custom station removed from database', !(await api('/api/radio')).some(s=>s.name==='Custom FM'));
  await js(`window.__radioDirectoryFail = false`);
  await clickText('.radio-directory button', 'Retry');
  check('directory retry recovers', await waitUntil(`!document.querySelector('.radio-directory [role="alert"]') && document.querySelector('.directory-station')`, 3000) < Infinity);
  await js(`window.__radioNextStation = true; document.querySelector('.radio-search').requestSubmit()`);
  await waitUntil(`document.querySelector('.directory-station')?.textContent.includes('Deferred FM')`, 3000);
  const selected = await js(`document.querySelector('.player-switch-btn').value`);
  const before = await js(`window.__radioPlaySends || 0`);
  await js(`window.__radioHoldSave = true`);
  await clickText('.directory-station button', 'Play');
  check('directory play awaits station save', await waitUntil(`window.__radioReleaseSave`, 3000) < Infinity);
  await js(`(() => {const s=document.querySelector('.player-switch-btn'); const option=[...s.options].find(o=>o.value !== s.value); s.value=option.value; s.dispatchEvent(new Event('change',{bubbles:true}));})()`);
  await js(`window.__radioReleaseSave?.()`);
  await waitUntil(`document.querySelector('.radio-saved')?.textContent.includes('Deferred FM')`, 3000);
  check('output switch cancels pending directory playback', await js(`(window.__radioPlaySends || 0) === ${before}`));
  await js(`(() => {const s=document.querySelector('.player-switch-btn'); s.value=${JSON.stringify(selected)}; s.dispatchEvent(new Event('change',{bubbles:true}));})()`);
  await screenshot('desktop-radio');
  await js(`window.__restoreDirectory()`);
}
