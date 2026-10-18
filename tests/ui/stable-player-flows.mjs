// #51/#53: navigation must not move the playback/output controls away from the listener.
export async function stablePlayerFlows({js, send, check, waitUntil, tap, screenshot}) {
  const settle = () => new Promise(r => setTimeout(r, 400));
  for (const [width,height] of [[390,844],[320,568],[844,390],[1024,768],[1440,900]]) {
    await send('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:width<1024});
    await js(`history.replaceState(null,'','/');localStorage.setItem('musicata.browse-view','library');localStorage.removeItem('musicata-vu-open')`);
    await send('Page.reload');
    await waitUntil('document.querySelector(".album-card")',5000);
    await settle();
    await js(`document.querySelector('.install-x')?.click()`);
    check(`Stable ${width}: search is directly reachable in Library`,await tap('[aria-label="Search library"]'));
    await js(`window.stableBounds=Object.fromEntries(['.transport','.output-trigger'].map(s=>[s,document.querySelector(s)?.getBoundingClientRect().toJSON()]));`);
    const stable = () => js(`['.transport','.output-trigger'].every(s=>{const a=window.stableBounds[s],b=document.querySelector(s)?.getBoundingClientRect();return a&&b&&['x','y','width','height'].every(k=>Math.abs(a[k]-b[k])<2)})`);
    check(`Stable ${width}: output picker reachable`,await tap('.output-trigger'));
    if(await js(`!!document.querySelector('.output-picker:popover-open')`)) {
      check(`Stable ${width}: picker identifies selected output`,await js(`!!document.querySelector('.output-picker [aria-pressed="true"]')`));
      await screenshot(`stable-${width}-output`);
      await send('Input.dispatchKeyEvent',{type:'keyDown',key:'Escape',code:'Escape'});
      await settle();
      check(`Stable ${width}: Escape restores output focus`,await js(`document.activeElement?.matches('.output-trigger') && !document.querySelector('.output-picker:popover-open')`));
    }
    if(width === 390) {
      await tap('.output-trigger');
      const name=await js(`document.querySelector('.output-option[data-output^="zone:"] strong')?.textContent`);
      check('Stable output: zone choice can be tapped',await tap('.output-option[data-output^="zone:"]',true));
      check('Stable output: chosen zone updates the persistent label',await waitUntil(`document.querySelector('.output-trigger strong')?.textContent === ${JSON.stringify(name)}`,2000)<Infinity);
      await tap('.output-trigger');
      check('Stable output: browser choice can be tapped',await tap('.output-option[data-output="player:browser-local"]',true));
      check('Stable output: chosen browser closes picker and restores focus',await js(`document.querySelector('.output-trigger strong')?.textContent === 'This Browser' && !document.querySelector('.output-picker:popover-open') && document.activeElement?.matches('.output-trigger')`));
    }
    check(`Stable ${width}: Now Playing opens`,await tap('.now-title-button'));
    await settle();
    check(`Stable ${width}: Now Playing keeps transport and output in place`,await stable());
    check(`Stable ${width}: queue is visible by default`,await js(`!!document.querySelector('.now-playing-panel .queue-drawer')?.getClientRects().length`));
    check(`Stable ${width}: playback modes are reachable without expanding options`,await js(`(()=>{const e=document.querySelector('.now-playing-panel [aria-label="Shuffle"]');if(!e)return false;const r=e.getBoundingClientRect();return e.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2));})()`));
    await js(`window.modeTop=document.querySelector('.playback-modes')?.getBoundingClientRect().top;document.querySelector('.now-playing-panel').scrollTop=500`);
    check(`Stable ${width}: playback modes stay fixed while queue scrolls`,await js(`Math.abs(document.querySelector('.playback-modes')?.getBoundingClientRect().top-window.modeTop)<2`));
    await js(`document.querySelector('.now-playing-panel').scrollTop=0`);
    await screenshot(`stable-${width}-now`);
    if(width<1024) {
      await tap('[aria-label="Open navigation"]');
      check(`Stable ${width}: navigation is above playback details`,await js(`(()=>{const e=document.querySelector('.library-nav button:not(.desktop-destination)');if(!e)return false;const r=e.getBoundingClientRect();return e.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2))})()`));
      await screenshot(`stable-${width}-navigation`);
      await tap('[aria-label="Close navigation"]');
      await settle();
    }
    await tap('[aria-label="Close Now Playing"]');
    check(`Stable ${width}: queue opens`,await tap(width<1024 ? '.mini-controls [aria-label="Queue"]' : '.queue-btn'));
    await settle();
    check(`Stable ${width}: queue keeps transport and output in place`,await stable());
    check(`Stable ${width}: playback stays reachable with queue open`,await js(`(()=>{const e=[...document.querySelectorAll('.transport [aria-label="Play"],.transport [aria-label="Pause"]')].find(e=>e.getClientRects().length);if(!e)return false;const r=e.getBoundingClientRect();return e.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2))&&!e.closest('[inert]')})()`));
    await screenshot(`stable-${width}-queue`);
    if(width === 1440) {
      await tap('.player-bar [title="Equalizer"]');
      await tap('.queue-btn');
      check('Stable panels: Queue brings queue forward from EQ',await tap('[aria-label="Close queue"]'));
      await tap('.player-bar [title="Equalizer"]');
      await tap('.now-title-button');
      check('Stable panels: Now Playing brings details forward from EQ',await tap('[aria-label="Close Now Playing"]'));
      await tap('.queue-btn');
    }
    if(width === 390) {
      await tap('.now-title-button');
      await settle();
      check('Stable panels: Now Playing replaces the visible queue',await tap('[aria-label="Close Now Playing"]'));
      await settle();
      check('Stable panels: Back from details restores queue',await tap('[aria-label="Close queue"]'));
      await settle();
      await tap('.now-title-button');
      await tap('.mini-controls [aria-label="Queue"]');
      check('Stable panels: covered details cannot take focus',await js(`!document.querySelector('.now-playing-panel')?.getClientRects().length || !!document.querySelector('.now-playing-panel')?.closest('[inert]')`));
      await tap('.now-title-button');
      check('Stable panels: an existing details view can be brought forward',await tap('[aria-label="Close Now Playing"]'));
      await settle();
      await tap('[aria-label="Close queue"]');
      await settle();
      await tap('[aria-label="Close Now Playing"]');
    } else {
      await tap('[aria-label="Close queue"]');
    }
    if(width<1024) await tap('[aria-label="Settings"]');
    else await tap('[data-activity="settings"]');
    await settle();
    check(`Stable ${width}: Settings keeps output in place`,await stable());
    check(`Stable ${width}: page fits viewport`,await js(`document.documentElement.scrollWidth<=document.documentElement.clientWidth`));
  }
}
