// Wizard coverage uses real player/zone persistence and simulated device discovery.
// No physical player or external MPD is required by the UI suite.
export async function addPlayerFlows({api, js, check, waitUntil, tap, send, screenshot}) {
  const before = await api('/api/players');
  const browser = before.find(player => player.id === 'browser-local');
  const click = text => js(`(() => { const root=document.querySelector('.add-player-wizard'); [...root.querySelectorAll('button')].find(button=>button.textContent.trim()===${JSON.stringify(text)})?.click(); })()`);
  const fill = (label,value) => js(`(() => { const root=document.querySelector('.add-player-wizard'); const input=[...root.querySelectorAll('label')].find(node=>node.textContent.trim().startsWith(${JSON.stringify(label)}))?.querySelector('input'); if(input){input.value=${JSON.stringify(value)}; input.dispatchEvent(new Event('input',{bubbles:true}));} })()`);
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
  for(const width of [1440,360]) {
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
    window.__wizardProbeFails=true;window.__wizardMatched=false;window.__wizardDeleted=0;
    window.__wizardPatchFails=true;window.__wizardProbeCount=0;window.__wizardProvisionCount=0;
    window.fetch=async(input,init={})=>{
      const url=input instanceof Request?input.url:String(input),method=(init.method||'GET').toUpperCase();
      if(url.endsWith('/api/players/probe')){
        window.__wizardProbeCount++;
        return new Response(JSON.stringify(window.__wizardProbeFails?{error:{code:'bad_request',message:'Could not connect to MPD. Check the address and password.'}}:{address:'127.0.0.1:16603',connected:true}),{status:window.__wizardProbeFails?400:200,headers:{'content-type':'application/json'}});
      }
      if(url.endsWith('/api/snapcast/players/provision')){
        window.__wizardProvisionCount++;
        return new Response(JSON.stringify({id:'musicata-wizard-test',command:"snapclient 'tcp://musicata-wizard-test:example-password@musicata.test:1704' --hostID 'musicata-wizard-test'",auth_enabled:true}),{headers:{'content-type':'application/json'}});
      }
      if(url.includes('/api/snapcast/rooms/musicata-wizard-test')&&method==='DELETE'){
        window.__wizardDeleted++;return new Response('[]',{headers:{'content-type':'application/json'}});
      }
      if(url.endsWith('/api/snapcast/status')&&method==='GET'){
        const response=await original(input,init);const base=await response.json();
        const client={id:window.__wizardMatched?'musicata-wizard-test':'unrelated-player',name:'Kitchen',hostname:'kitchen',ip_address:'192.0.2.36',connected:true,zone_id:null,volume_percent:50,muted:false};
        return new Response(JSON.stringify({...base,enabled:true,running:true,server_host:'musicata.test',auth_enabled:true,clients:[client],health:{...base.health,control:'connected',clients:[client]}}),{headers:{'content-type':'application/json'}});
      }
      if(url.includes('/api/players/')&&method==='PATCH'&&JSON.parse(init.body||'{}').name==='Wizard MPD'&&window.__wizardPatchFails){window.__wizardPatchFails=false;return new Response(JSON.stringify({error:{message:'Temporary save failure'}}),{status:503,headers:{'content-type':'application/json'}});}
      if(url.includes('/api/snapcast/clients/musicata-wizard-test')&&method==='PATCH'){
        window.__wizardSaved=JSON.parse(init.body);return new Response('{}',{headers:{'content-type':'application/json'}});
      }
      return original(input,init);
    };
  })()`);
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
    await waitUntil(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Prepare player')`,5000);
    await click('Prepare player');
    check('Add player: provisioning returns a credential command',await waitUntil(`document.querySelector('.add-player-wizard code')?.textContent.includes('example-password')`,3000)<Infinity);
    await click('Check for this player');
    await new Promise(resolve=>setTimeout(resolve,400));
    check('Add player: unrelated connected speaker cannot complete setup',await js(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&button.disabled)`));
    await js('window.__wizardMatched=true');
    await click('Check for this player');
    check('Add player: matches its provisioned identity',await waitUntil(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Continue'&&!button.disabled)`,3000)<Infinity);
    await click('Continue');
    await fill('Player name','Wizard speaker');
    await click('Add player');
    check('Add player: names the matched Snapcast player',await waitUntil(`window.__wizardSaved?.name==='Wizard speaker'`,3000)<Infinity);
    await waitUntil(`!document.querySelector('.add-player-wizard')`,10000);
    await open();
    await click('Continue');
    if(await js(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Enable Snapcast')`)) await click('Enable Snapcast');
    await waitUntil(`[...document.querySelectorAll('.add-player-wizard button')].some(button=>button.textContent.trim()==='Prepare player')`,5000);
    await click('Prepare player');
    await waitUntil(`document.querySelector('.add-player-wizard code')`,3000);
    await click('Cancel');
    check('Add player: cancellation preserves a newly connected speaker',await waitUntil(`document.querySelector('.add-player-wizard')?.textContent.includes('connected while')`,3000)<Infinity && await js('window.__wizardDeleted===0'));
    await click('Cancel');
    await open();
    await click('Continue');
    await click('Prepare player');
    await waitUntil(`document.querySelector('.add-player-wizard code')`,3000);
    await js('window.__wizardMatched=false');
    await click('Cancel');
    check('Add player: cancels only its unconnected provisioning',await waitUntil(`window.__wizardDeleted===1`,3000)<Infinity);
  } finally {
    await js(`window.fetch=window.__wizardFetch;delete window.__wizardFetch`);
    await api('/api/players/browser-local',{method:'PATCH',headers:{'content-type':'application/json'},body:JSON.stringify({name:browser.name,zone_id:browser.zone_id})});
    for(const player of await api('/api/players'))if(player.name==='Wizard MPD')await api('/api/players/'+player.id,{method:'DELETE'});
    for(const zone of await api('/api/zones'))if(zone.name==='Wizard zone')await api('/api/zones/'+zone.id,{method:'DELETE'});
    await js(`document.querySelector('[data-activity="browse"]')?.click()`);
  }
}
