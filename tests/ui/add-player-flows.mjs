// Wizard coverage uses real player/zone persistence and simulated device discovery.
// No physical player or external MPD is required by the UI suite.
export async function addPlayerFlows({api, js, check, waitUntil, tap, send, screenshot}) {
  const before = await api('/api/players');
  const browser = before.find(player => player.id === 'browser-local');
  const click = text => js(`(() => { const root=document.querySelector('.add-player-wizard'); [...root.querySelectorAll('button')].find(button=>button.textContent.trim()===${JSON.stringify(text)})?.click(); })()`);
  const fill = (label,value) => js(`(() => { const root=document.querySelector('.add-player-wizard'); const input=[...root.querySelectorAll('label')].find(node=>node.textContent.trim().startsWith(${JSON.stringify(label)}))?.querySelector('input'); if(input){input.value=${JSON.stringify(value)}; input.dispatchEvent(new Event('input',{bubbles:true}));} })()`);
  const selectSnapcast = async id => {
    const candidate = `(()=>{const input=[...document.querySelectorAll('.add-player-wizard input[name="snapcast-client"]')].find(input=>input.value===${JSON.stringify(id)});return !!input&&!input.matches(':disabled');})()`;
    if(await waitUntil(candidate,3000)===Infinity)return false;
    return await js(`(() => { const input=[...document.querySelectorAll('.add-player-wizard input[name="snapcast-client"]')].find(input=>input.value===${JSON.stringify(id)});if(!input||input.matches(':disabled'))return false;input.click();return input.checked; })()`);
  };
  const open = async () => {
    await js(`[...document.querySelectorAll('.players-panel button')].find(button=>button.textContent.trim()==='Add player')?.click()`);
    return await waitUntil(`document.querySelector('.add-player-wizard')`,3000)<Infinity;
  };
  await tap('[aria-label="Settings"]');
  await waitUntil(`document.querySelector('[data-settings-category="outputs"]')`,3000);
  await js(`document.querySelector('[data-settings-category="outputs"]')?.click()`);
  await waitUntil(`document.querySelector('.players-panel')`,3000);
  const opened=await open();
  check('Add player: opens the wizard from Zones & players',opened);
  if(!opened){await js(`document.querySelector('[data-activity="browse"]')?.click()`);return;}
  for(const width of [1440,390]) {
    await send('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:width<720});
    await js(`document.querySelector('.add-player-wizard')?.scrollIntoView({block:'start'})`);
    await new Promise(resolve=>setTimeout(resolve,200));
    check(`Add player: ${width}px has no horizontal overflow`,await js(`document.documentElement.scrollWidth<=innerWidth`));
    await screenshot(`wizard-${width}`);
  }
  await send('Emulation.setDeviceMetricsOverride',{width:1280,height:900,deviceScaleFactor:1,mobile:false});
  check('Add player: native endpoint is absent',await js(`!document.querySelector('.add-player-wizard').textContent.includes('native player')`));
  await js(`(() => {
    const original=window.fetch.bind(window);window.__wizardFetch=original;
    window.__wizardProbeFails=true;window.__wizardPatchFails=true;window.__wizardProbeCount=0;
    window.__wizardProvisionMutations=0;window.__wizardSaved={};window.__wizardSnapcastPatches=[];window.__wizardSnapcastAssignFails=true;
    window.__wizardDiscovery='available';
    window.fetch=async(input,init={})=>{
      const url=input instanceof Request?input.url:String(input),method=(init.method||'GET').toUpperCase();
      if((url.includes('/api/snapcast/players/provision')||url.includes('/api/snapcast/rooms'))&&(method==='POST'||method==='DELETE')){
        window.__wizardProvisionMutations++;return original(input,init);
      }
      if(url.endsWith('/api/players/probe')){
        window.__wizardProbeCount++;
        return new Response(JSON.stringify(window.__wizardProbeFails?{error:{code:'bad_request',message:'Could not connect to MPD. Check the address and password.'}}:{address:'127.0.0.1:16603',connected:true}),{status:window.__wizardProbeFails?400:200,headers:{'content-type':'application/json'}});
      }
      if(url.endsWith('/api/snapcast/status')&&method==='GET'){
        const response=await original(input,init);const base=await response.json();
        const kitchen={id:'f4f3e2d1-c0b9-a8f7-e6d5-c4b3a2918076',name:'Kitchen device',hostname:'kitchen',ip_address:'192.0.2.36',connected:true,zone_id:null,volume_percent:50,muted:false};
        const office={id:'01234567-89ab-cdef-0123-456789abcdef',name:'Office device',hostname:'office',ip_address:'192.0.2.37',connected:true,zone_id:null,volume_percent:50,muted:false};
        const assigned={id:'already-assigned',name:'Existing room',hostname:'living-room',ip_address:'192.0.2.38',connected:true,zone_id:'existing-zone',volume_percent:50,muted:false};
        const disconnected={id:'offline-client',name:'Offline device',hostname:'offline',ip_address:'192.0.2.39',connected:false,zone_id:null,volume_percent:50,muted:false};
        if(window.__wizardDiscovery==='selected-offline') kitchen.connected=false;
        if(window.__wizardDiscovery==='selected-assigned') kitchen.zone_id='existing-zone';
        const clients=window.__wizardDiscovery==='selected-gone'?[office,assigned,disconnected]:[kitchen,office,assigned,disconnected];
        const control=window.__wizardDiscovery==='unavailable'?'unavailable':window.__wizardDiscovery==='stale'?'stale':'connected';
        const rawClients=clients.map(client=>({...client,zone_id:null}));
        return new Response(JSON.stringify({...base,enabled:true,running:true,server_host:'musicata.test',auth_enabled:false,clients,health:{...base.health,control,clients:rawClients}}),{headers:{'content-type':'application/json'}});
      }
      if(url.includes('/api/players/')&&method==='PATCH'&&JSON.parse(init.body||'{}').name==='Wizard MPD'&&window.__wizardPatchFails){window.__wizardPatchFails=false;return new Response(JSON.stringify({error:{message:'Temporary save failure'}}),{status:503,headers:{'content-type':'application/json'}});}
      const snapcastClientId=url.split('/').pop();
      if(url.includes('/api/snapcast/clients/')&&['f4f3e2d1-c0b9-a8f7-e6d5-c4b3a2918076','01234567-89ab-cdef-0123-456789abcdef'].includes(snapcastClientId)&&method==='PATCH'){
        const id=snapcastClientId,body=JSON.parse(init.body);window.__wizardSnapcastPatches.push({id,...body});
        if(body.name!==undefined&&body.zone_id!==undefined)return new Response(JSON.stringify({error:{message:'Update player name and zone separately.'}}),{status:400,headers:{'content-type':'application/json'}});
        if(id==='f4f3e2d1-c0b9-a8f7-e6d5-c4b3a2918076'&&body.zone_id!==undefined&&window.__wizardSnapcastAssignFails){window.__wizardSnapcastAssignFails=false;return new Response(JSON.stringify({error:{message:'Temporary Snapcast zone failure'}}),{status:503,headers:{'content-type':'application/json'}});}
        window.__wizardSaved[id]={...(window.__wizardSaved[id]||{}),...body};return new Response('{}',{headers:{'content-type':'application/json'}});
      }
      return original(input,init);
    };
  })()`);
  check('Add player: discovery transport mock is installed',await js(`typeof window.__wizardFetch==='function'&&typeof window.fetch==='function'`));
  try {
    await js(`document.querySelector('.add-player-wizard input[data-player-kind="mpd"]')?.click()`);
    await click('Continue');
    await fill('Hostname or IP address','127.0.0.1');
    await fill('Port','16603');
    await fill('Password','fixture-password');
    await click('Test connection');
    check('Add player: failed MPD probe explains retry',await waitUntil(`document.querySelector('.add-player-wizard').textContent.includes('Check the address')`,3000)<Infinity);
    check('Add player: failed probe creates no player',(await api('/api/players')).length===before.length);
    await js('window.__wizardProbeFails=false');
    await click('Test connection');
    check('Add player: MPD probe can retry',await waitUntil(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&!button.disabled)`,3000)<Infinity);
    await click('Continue');
    await fill('Player name','Wizard MPD');
    await js(`(() => {const select=document.querySelector('.add-player-wizard select');select.value='new';select.dispatchEvent(new Event('change',{bubbles:true}));})()`);
    await fill('New zone name','Wizard zone');
    await click('Add player');
    check('Add player: partial save can retry',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('Temporary save failure')`,10000)<Infinity);
    check('Add player: partial save locks its created identity',await js(`[...document.querySelectorAll('.add-player-wizard button')].find(button=>button.textContent.trim()==='Back')?.disabled && document.querySelector('.add-player-wizard select')?.disabled`));
    await click('Add player');
    check('Add player: saves MPD once',await waitUntil(`!document.querySelector('.add-player-wizard')`,15000)<Infinity);
    const added=(await api('/api/players')).filter(player=>player.name==='Wizard MPD');
    check('Add player: one persisted MPD identity',added.length===1&&added[0].kind==='mpd');
    const wizardZone=(await api('/api/zones')).find(zone=>zone.name==='Wizard zone');
    check('Add player: creates and assigns a zone',!!wizardZone && added[0]?.zone_id===wizardZone.id);
    await waitUntil(`!document.querySelector('.add-player-wizard')`,10000);
    await open();
    check('Add player: a new wizard remains interactive while the previous refresh finishes',await js(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&!button.disabled)`));
    await js(`document.querySelector('.add-player-wizard input[data-player-kind="browser"]')?.click()`);
    await click('Continue');
    await click('Continue');
    await fill('Player name','Wizard browser');
    await click('Save player');
    await waitUntil(`!document.querySelector('.add-player-wizard')`,15000);
    const renamed=await api('/api/players');
    check('Add player: browser reuses its identity',renamed.length===before.length+added.length&&renamed.find(player=>player.id==='browser-local')?.name==='Wizard browser');
    await waitUntil(`!document.querySelector('.add-player-wizard')`,10000);
    await open();
    await click('Continue');
    if(await js(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Enable Snapcast')`)) await click('Enable Snapcast');
    for(const width of [1440,390]) {
      await send('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:width<720});
      await js(`document.querySelector('.add-player-wizard')?.scrollIntoView({block:'start'})`);
      await new Promise(resolve=>setTimeout(resolve,200));
      check(`Add player connect: ${width}px has no horizontal overflow`,await js(`document.documentElement.scrollWidth<=innerWidth`));
      await screenshot(`wizard-connect-${width}`);
    }
    await send('Emulation.setDeviceMetricsOverride',{width:1280,height:900,deviceScaleFactor:1,mobile:false});
    check('Add player: shows source installer guidance without a credential command',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('install-player.sh --server')`,3000)<Infinity && await js(`!document.querySelector('.add-player-wizard code')?.textContent.includes('example-password')`));
    await click('Refresh players');
    check('Add player: lists only connected unassigned Snapclients',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('Kitchen device')`,3000)<Infinity && await js(`document.querySelector('.add-player-wizard')?.textContent.includes('Office device')&&!document.querySelector('.add-player-wizard')?.textContent.includes('Existing room')&&!document.querySelector('.add-player-wizard')?.textContent.includes('Offline device')`));
    check('Add player: identifies discovered machines by stable UUID',await js(`document.querySelector('.add-player-wizard')?.textContent.includes('f4f3e2d1-c0b9-a8f7-e6d5-c4b3a2918076')&&document.querySelector('.add-player-wizard')?.textContent.includes('01234567-89ab-cdef-0123-456789abcdef')`));
    check('Add player: requires an explicit Snapclient selection',await js(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&button.disabled)`));
    check('Add player: discovered client can be selected',await selectSnapcast('f4f3e2d1-c0b9-a8f7-e6d5-c4b3a2918076'));
    check('Add player: enables continue after selecting a discovered client',await waitUntil(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&!button.disabled)`,3000)<Infinity);
    await js(`window.__wizardDiscovery='unavailable'`);
    await click('Refresh players');
    check('Add player: control outage prevents completing discovery',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('unavailable')`,3000)<Infinity && await js(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&button.disabled)`));
    await js(`window.__wizardDiscovery='stale'`);
    await click('Refresh players');
    check('Add player: stale discovery prevents completing setup',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('unavailable')`,3000)<Infinity && await js(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&button.disabled)`));
    await js(`window.__wizardDiscovery='selected-gone'`);
    await click('Refresh players');
    check('Add player: refresh never substitutes another discovered player',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('no longer')`,3000)<Infinity && await js(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&button.disabled)`));
    await js(`window.__wizardDiscovery='available'`);
    await click('Refresh players');
    check('Add player: refreshed client can be selected',await selectSnapcast('f4f3e2d1-c0b9-a8f7-e6d5-c4b3a2918076'));
    await js(`window.__wizardDiscovery='selected-offline'`);
    await click('Refresh players');
    check('Add player: an offline selected client cannot be replaced',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('no longer')`,3000)<Infinity && await js(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&button.disabled)`));
    await js(`window.__wizardDiscovery='available'`);
    await click('Refresh players');
    check('Add player: restored client can be selected',await selectSnapcast('f4f3e2d1-c0b9-a8f7-e6d5-c4b3a2918076'));
    await click('Continue');
    check('Add player: selected client reaches name and zone',await waitUntil(`document.querySelector('.add-player-wizard select')&&[...document.querySelectorAll('.add-player-wizard label span')].some(label=>label.textContent==='Player name')`,3000)<Infinity);
    await fill('Player name','Wizard speaker');
    check('Add player: default Snapcast choice explains shared playback',await js(`document.querySelector('.add-player-wizard select')?.selectedOptions[0]?.textContent.includes('Shared playback')&&document.querySelector('.add-player-wizard')?.textContent.includes('existing default queue')`));
    await js(`window.__wizardDiscovery='selected-assigned'`);
    await click('Add player');
    check('Add player: final save rejects a client assigned elsewhere without choosing another',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('no longer')`,3000)<Infinity && await js(`Object.keys(window.__wizardSaved).length===0`));
    await js(`window.__wizardDiscovery='available'`);
    await js(`(() => {const select=document.querySelector('.add-player-wizard select');select.value=${JSON.stringify(wizardZone.id)};select.dispatchEvent(new Event('change',{bubbles:true}));})()`);
    await click('Add player');
    check('Add player: Snapcast zone failure saves only Kitchen name',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('Temporary Snapcast zone failure')`,3000)<Infinity && await js(`window.__wizardSnapcastPatches.length===2&&window.__wizardSnapcastPatches[0].id==='f4f3e2d1-c0b9-a8f7-e6d5-c4b3a2918076'&&window.__wizardSnapcastPatches[0].name==='Wizard speaker'&&window.__wizardSnapcastPatches[0].zone_id===undefined&&window.__wizardSnapcastPatches[1].id==='f4f3e2d1-c0b9-a8f7-e6d5-c4b3a2918076'&&window.__wizardSnapcastPatches[1].name===undefined&&window.__wizardSnapcastPatches[1].zone_id`));
    await click('Back');
    check('Add player: partial Snapcast save can return to discovery',await waitUntil(`document.querySelector('.add-player-wizard input[name="snapcast-client"]')`,3000)<Infinity);
    check('Add player: replacement client can be selected after partial save',await selectSnapcast('01234567-89ab-cdef-0123-456789abcdef'));
    await click('Continue');
    check('Add player: replacement client reaches name and zone',await waitUntil(`document.querySelector('.add-player-wizard select')`,3000)<Infinity);
    await click('Add player');
    check('Add player: replacement Snapclient receives separate name and zone updates',await waitUntil(`window.__wizardSaved['01234567-89ab-cdef-0123-456789abcdef']?.name==='Wizard speaker'&&window.__wizardSaved['01234567-89ab-cdef-0123-456789abcdef']?.zone_id`,3000)<Infinity && await js(`window.__wizardSnapcastPatches.filter(patch=>patch.id==='01234567-89ab-cdef-0123-456789abcdef'&&patch.name!==undefined).length===1&&window.__wizardSnapcastPatches.filter(patch=>patch.id==='01234567-89ab-cdef-0123-456789abcdef'&&patch.zone_id!==undefined).length===1&&window.__wizardSnapcastPatches.every(patch=>patch.name===undefined||patch.zone_id===undefined)`));
    await waitUntil(`!document.querySelector('.add-player-wizard')`,10000);
    await open();
    await click('Continue');
    check('Add player: cancellation waits for initial discovery',await waitUntil(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Refresh players'&&!button.disabled)`,3000)<Infinity);
    await click('Refresh players');
    check('Add player: cancellation waits for refresh',await waitUntil(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Refresh players'&&!button.disabled)`,3000)<Infinity);
    await click('Cancel');
    check('Add player: cancellation makes no server provisioning mutation',await waitUntil(`!document.querySelector('.add-player-wizard')`,3000)<Infinity && await js('window.__wizardProvisionMutations===0'));
  } finally {
    await js(`window.fetch=window.__wizardFetch;delete window.__wizardFetch`);
    await api('/api/players/browser-local',{method:'PATCH',headers:{'content-type':'application/json'},body:JSON.stringify({name:browser.name,zone_id:browser.zone_id})});
    for(const player of await api('/api/players'))if(player.name==='Wizard MPD')await api('/api/players/'+player.id,{method:'DELETE'});
    for(const zone of await api('/api/zones'))if(zone.name==='Wizard zone')await api('/api/zones/'+zone.id,{method:'DELETE'});
    await js(`document.querySelector('[data-activity="browse"]')?.click()`);
  }
}
