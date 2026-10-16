// Regression journeys for #51/#53; operate only on the disposable smoke library.
export async function uxFlows({js, send, check, waitUntil, tap, screenshot}) {
  const settle = () => new Promise(r => setTimeout(r, 400));
  for (const [width,height] of [[390,844],[320,568],[844,390],[1440,900]]) {
    await send('Emulation.setDeviceMetricsOverride', {width,height,deviceScaleFactor:1,mobile:width<1024});
    await js(`history.replaceState(null, '', '/'); localStorage.removeItem('musicata.browse-view'); localStorage.removeItem('musicata-vu-open')`);
    await send('Page.reload');
    await waitUntil('document.querySelector(".album-card") || document.querySelector(".track")',5000);
    await settle();
    await js(`document.querySelector('.install-x')?.click()`);
    const compact = width < 1024;
    check(`UX ${width}: library has useful width`, await js(`document.querySelector('.content').getBoundingClientRect().width > ${compact ? width*.85 : width*.60}`));
    if(width === 390) {
      await js(`document.querySelectorAll('.seg')[0]?.click()`);
      await waitUntil('document.querySelectorAll(".track").length > 5',3000);
      await js(`window.scrollTo(0,240);document.querySelector('.content')?.scrollTo(0,240)`);
      await settle();
      await js(`window.uxBrowseScroll={page:scrollY,content:document.querySelector('.content').scrollTop};document.querySelector('.track-album-cell')?.click()`);
      await waitUntil('document.querySelector(".hero-title")',3000);
    }
    if (compact) await tap('[aria-label="Open navigation"]');
    await js(`(()=>{const input=document.querySelector('.search input'); input.value='Dar'; input.dispatchEvent(new Event('input',{bubbles:true})); input.form?.requestSubmit();})()`);
    await settle();
    check(`UX ${width}: search submission exposes results`, await js(`!document.querySelector('.shell').classList.contains('nav-open') && !!document.querySelector('[aria-label="Clear search"]')`));
    const clear = await tap('[aria-label="Clear search"]');
    check(`UX ${width}: clear search is reachable`, clear);
    if (clear) {
      await settle();
      check(`UX ${width}: clear restores unfiltered browsing`, await js(`!document.querySelector('.content-title')?.textContent.includes('Search:') && document.querySelector('.search input')?.value === ''`));
    }
    if(width === 390) check('UX search: detail search restores prior list scroll', await waitUntil(`Math.abs(scrollY-window.uxBrowseScroll.page)<2 && Math.abs(document.querySelector('.content').scrollTop-window.uxBrowseScroll.content)<2`,3000)<Infinity);
    await js(`window.scrollTo(0,0);document.querySelector('.content')?.scrollTo(0,0)`);
    await js(`document.querySelectorAll('.seg')[0]?.click()`);
    await waitUntil('document.querySelector(".track")',3000);
    check(`UX ${width}: compact track rows`,await js(`document.querySelector('.track')?.getBoundingClientRect().height < 120`));
    check(`UX ${width}: primary track target`,await js(`document.querySelector('.track-play')?.getBoundingClientRect().height >= 44`));
    await settle();
    const menu = await tap('.track-menu-toggle', true);
    check(`UX ${width}: track actions reachable`, menu);
    if(menu) {
      check(`UX ${width}: actions have visible labels`, await js(`document.querySelector('.track-menu[open]')?.textContent.includes('Play next') && document.querySelector('.track-menu[open]')?.textContent.includes('Edit metadata')`));
      await screenshot(`ux-${width}-actions`);
      await send('Input.dispatchKeyEvent',{type:'keyDown',key:'Escape',code:'Escape'});
      await settle();
      check(`UX ${width}: Escape closes actions`,await js(`!document.querySelector('.track-menu[open]')`));
    }
    // A just-typed debounce must not resurrect search after choosing Library/Browse.
    if(compact) await tap('[aria-label="Open navigation"]');
    await js(`(()=>{const input=document.querySelector('.search input'); input.value='pending'; input.dispatchEvent(new Event('input',{bubbles:true})); document.querySelector(${JSON.stringify(compact ? '.mobile-tabs [data-tab="library"]' : '[data-activity="browse"]')})?.click();})()`);
    await settle();
    check(`UX ${width}: leaving search cancels pending input`,await js(`document.querySelector('.search input')?.value === '' && !document.querySelector('.clear-search')`));
    await js(`(()=>{const input=document.querySelector('.search input');input.value='';input.dispatchEvent(new Event('input',{bubbles:true}));})()`);
    await settle();
    if(!compact) {
      await js(`document.querySelector('.track:last-child')?.scrollIntoView({block:'end'})`);
      await settle();
      const opened=await tap('.track:last-child .track-menu-toggle');
      check('UX desktop: last track actions stay in viewport',opened && await js(`(()=>{const e=document.querySelector('.track-menu[open] .track-menu-content');if(!e)return false;const r=e.getBoundingClientRect();return r.top>=0&&r.bottom<=innerHeight&&e.contains(document.elementFromPoint(r.x+r.width/2,r.bottom-10));})()`));
      await screenshot('ux-desktop-last-actions');
      await send('Input.dispatchKeyEvent',{type:'keyDown',key:'Escape',code:'Escape'});
      await js(`document.querySelector('.content')?.scrollTo({top:0})`);
    }
    await screenshot(`ux-${width}-tracks`);
    await js(`document.querySelectorAll('.seg')[1]?.click()`);
    await waitUntil('document.querySelector(".album-card")',3000);
    await tap('.album-card');
    await waitUntil('document.querySelector(".hero-title")',3000);
    check(`UX ${width}: album title appears once`,await js(`!!document.querySelector('.hero-title') && !document.querySelector('.content-title h2')`));
    if(compact) check(`UX ${width}: album tracks start near header`,await js(`document.querySelector('.track')?.getBoundingClientRect().top < 400`));
    await screenshot(`ux-${width}-album`);
    if(compact) {
      await tap('[aria-label="Settings"]');
      await settle();
      check(`UX ${width}: compact settings category selector`, await js(`document.querySelector('.settings-category-select')?.getBoundingClientRect().height >= 44`));
      await js(`(()=>{const s=document.querySelector('.settings-category-select');if(s){s.value='outputs';s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
      await settle();
      check(`UX ${width}: player controls contained`,await js(`document.querySelectorAll('.players-panel .admin-row').length > 0 && Array.from(document.querySelectorAll('.players-panel .admin-row')).every(row=>Array.from(row.querySelectorAll('button,select,strong')).every(el=>{const a=row.getBoundingClientRect(),b=el.getBoundingClientRect();return b.right<=a.right+1&&b.left>=a.left-1}))`));
      await screenshot(`ux-${width}-settings`);
    }
  }
  // Queue editing is opt-in, and toggling the UI must not change queue content.
  await js(`document.querySelectorAll('.seg')[0]?.click()`);
  await waitUntil('document.querySelector(".track-play")',3000);
  await tap('.track-play');
  await tap('.now-title-button');
  await waitUntil('document.querySelector(".queue-row")',3000);
  const titles=await js(`Array.from(document.querySelectorAll('.q-title')).map(e=>e.textContent)`);
  check('UX queue: editing controls initially hidden',await js(`Array.from(document.querySelectorAll('.q-actions')).every(e=>e.hidden)`));
  check('UX queue: edit action is reachable',await tap('.queue-edit'));
  check('UX queue: editing exposes reorder controls',await js(`!!document.querySelector('.q-actions:not([hidden]) button')`));
  await tap('.queue-edit');
  check('UX queue: editing preserves content',JSON.stringify(titles)===JSON.stringify(await js(`Array.from(document.querySelectorAll('.q-title')).map(e=>e.textContent)`)));
  await screenshot('ux-desktop-now-playing');
}

