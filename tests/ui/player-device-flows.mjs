// Permanent MPD identity, connection editing, and configured-output UI coverage. The MPD
// endpoint is deliberately offline: saving a connection must not require sound hardware.
export async function playerDeviceFlows({api, js, check, waitUntil, send, screenshot}) {
  const name = 'Device flow MPD';
  const created = await api('/api/players', {
    method: 'POST', headers: {'content-type': 'application/json'},
    body: JSON.stringify({kind: 'mpd', address: '127.0.0.1:16611', name}),
  });
  if (!created?.id) {
    check('Player device: creates temporary MPD player', false, 'registration failed');
    return;
  }
  const id = created.id;
  const row = `([...document.querySelectorAll('.zone-member')].find(row => row.textContent.includes(${JSON.stringify(name)})))`;
  const inRow = (expression) => `${row} && (() => { const row=${row}; ${expression} })()`;

  try {
    await js(`document.querySelector('[data-activity="settings"]')?.click()`);
    await waitUntil(`document.querySelector('[data-settings-category="outputs"]')`, 3000);
    await js(`document.querySelector('[data-settings-category="outputs"]')?.click()`);
    check('Player device: temporary MPD row is visible', await waitUntil(row, 5000) < Infinity);

    await js(inRow(`[...row.querySelectorAll('button')].find(button => button.textContent.trim() === 'Connection')?.click(); return true;`));
    check('Player device: opens connection editor', await waitUntil(`${row}?.querySelector('.connection-editor')`, 3000) < Infinity);
    await js(inRow(`const inputs=row.querySelectorAll('.connection-editor input'); inputs[0].value='127.0.0.1'; inputs[0].dispatchEvent(new Event('input',{bubbles:true})); inputs[1].value='16612'; inputs[1].dispatchEvent(new Event('input',{bubbles:true})); return true;`));
    await js(inRow(`[...row.querySelectorAll('.connection-editor button')].find(button => button.textContent.trim() === 'Save connection')?.click(); return true;`));
    check('Player device: saves an offline connection without replacing the row', await waitUntil(`${row}?.textContent.includes('reconnects in the background')`, 5000) < Infinity);
    const rebound = (await api('/api/players')).find(player => player.id === id);
    check('Player device: connection edit keeps stable player ID', rebound?.id === id && rebound?.address === '127.0.0.1:16612');

    await js(`(() => {
      window.__deviceFetch = window.fetch.bind(window);
      window.__deviceMode = 'normal'; window.__devicePut = null;
      const normal = {outputs:[
        {id:1,name:'Built-in',plugin:'alsa',enabled:true},
        {id:2,name:'USB DAC',plugin:'alsa',enabled:false},
      ],selected:{name:'Built-in',plugin:'alsa'},selection_supported:true,reason:null};
      window.fetch = async (input, init = {}) => {
        const url = input instanceof Request ? input.url : String(input);
        const method = (init.method || 'GET').toUpperCase();
        if (url.endsWith('/api/players/${id}/outputs')) {
          if (method === 'PUT') {
            window.__devicePut = JSON.parse(init.body || '{}');
            const selected = window.__devicePut;
            return new Response(JSON.stringify({...normal, selected, outputs:normal.outputs.map(output => ({...output,enabled:output.name===selected.name&&output.plugin===selected.plugin}))}), {headers:{'content-type':'application/json'}});
          }
          if (window.__deviceMode === 'duplicate') return new Response(JSON.stringify({outputs:[{id:3,name:'Duplicated',plugin:'alsa',enabled:true},{id:4,name:'Duplicated',plugin:'alsa',enabled:false}],selected:null,selection_supported:true,reason:null}), {headers:{'content-type':'application/json'}});
          if (window.__deviceMode === 'unsupported') return new Response(JSON.stringify({outputs:[],selected:null,selection_supported:false,reason:'DSP processor controls this MPD output.'}), {headers:{'content-type':'application/json'}});
          return new Response(JSON.stringify(normal), {headers:{'content-type':'application/json'}});
        }
        return window.__deviceFetch(input, init);
      };
    })()`);

    await js(inRow(`row.querySelector('.audio-device summary')?.click(); [...row.querySelectorAll('.audio-device button')].find(button => button.textContent.trim() === 'Refresh outputs')?.click(); return true;`));
    check('Player device: shows configured outputs, including disabled output', await waitUntil(`${row}?.textContent.includes('USB DAC')`, 3000) < Infinity && await js(inRow(`const label=[...row.querySelectorAll('.output-options label')].find(label=>label.textContent.includes('USB DAC')); return !!label?.querySelector('input:not(:disabled)');`)));
    await js(inRow(`[...row.querySelectorAll('.output-options label')].find(label => label.textContent.includes('USB DAC'))?.querySelector('input')?.click(); return true;`));
    check('Player device: can select a disabled configured output', await js(inRow(`return row.querySelector('.output-options input:checked')?.parentElement?.textContent.includes('USB DAC');`)));
    await js(inRow(`[...row.querySelectorAll('.audio-device button')].find(button => button.textContent.trim() === 'Apply audio device')?.click(); return true;`));
    check('Player device: applies by configured name and plugin', await waitUntil(`window.__devicePut !== null`, 3000) < Infinity && await js(`window.__devicePut?.name === 'USB DAC' && window.__devicePut?.plugin === 'alsa'`));

    for (const [width, height] of [[1440, 900], [390, 844]]) {
      await send('Emulation.setDeviceMetricsOverride', {width, height, deviceScaleFactor: 1, mobile: width < 720});
      await js(`${row}?.scrollIntoView({block:'start'})`);
      check(`Player device: ${width}px has no horizontal overflow`, await js('document.documentElement.scrollWidth <= innerWidth'));
      await screenshot(`player-device-${width}`);
      await js(`${row}?.querySelector('.audio-device')?.scrollIntoView({block:'start'})`);
      await screenshot(`player-device-outputs-${width}`);
    }
    await send('Emulation.setDeviceMetricsOverride', {width: 1280, height: 900, deviceScaleFactor: 1, mobile: false});

    await js(`window.__deviceMode = 'duplicate'`);
    await js(inRow(`[...row.querySelectorAll('.audio-device button')].find(button => button.textContent.trim() === 'Refresh outputs')?.click(); return true;`));
    check('Player device: rejects ambiguous duplicate configured outputs', await waitUntil(`${row}?.textContent.includes('Ambiguous duplicate')`, 3000) < Infinity && await js(inRow(`return [...row.querySelectorAll('.output-options input')].every(input => input.disabled) && [...row.querySelectorAll('.audio-device button')].find(button => button.textContent.trim() === 'Apply audio device')?.disabled;`)));

    await js(`window.__deviceMode = 'unsupported'`);
    await js(inRow(`[...row.querySelectorAll('.audio-device button')].find(button => button.textContent.trim() === 'Refresh outputs')?.click(); return true;`));
    check('Player device: shows DSP-bound output reason without a false empty-output claim', await waitUntil(`${row}?.textContent.includes('DSP processor controls')`, 3000) < Infinity && await js(inRow(`return !row.textContent.includes('MPD did not report any configured outputs');`)));

  } finally {
    await js(`if(window.__deviceFetch){window.fetch=window.__deviceFetch;delete window.__deviceFetch;delete window.__deviceMode;delete window.__devicePut;}`);
    await api(`/api/players/${id}`, {method: 'DELETE'});
    await js(`document.querySelector('[data-activity="browse"]')?.click()`);
  }
}
