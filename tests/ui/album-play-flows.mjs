// Album-card playback primes its first stream inside the gesture and keeps the chosen output.
export async function albumPlayFlows({js, check, waitUntil, tap}) {
  await js(`(() => {
    window.__albumOriginalFetch = window.fetch;
    window.__albumCommands = [];
    window.fetch = async (...args) => {
      if (args[1]?.body) {
        try { const body=JSON.parse(args[1].body); if(body.command==='play_tracks') window.__albumCommands.push(String(args[0])); } catch {}
      }
      const response = await window.__albumOriginalFetch(...args);
      if (/\\/api\\/albums\\/[^/?]+$/.test(String(args[0]))) return new Promise(resolve => { window.__releaseAlbum=()=>resolve(response); });
      return response;
    };
  })()`);
  check('Album card: play is reachable', await tap('.album-card .card-play'));
  check('Album card: detail response is held', await waitUntil(`!!window.__releaseAlbum`,2000)<Infinity);
  check('Album card: audio starts before the album fetch completes', await waitUntil(`!document.querySelector('audio').paused && document.querySelector('audio').currentTime>0`,2500)<Infinity);
  // Switching outputs while metadata loads must not queue the album on either target.
  await js(`document.querySelector('.output-option[data-output^="zone:"]')?.click()`);
  await js(`window.__releaseAlbum?.()`);
  await new Promise(resolve=>setTimeout(resolve,400));
  check('Album card: switching output cancels the pending queue replacement', await js(`window.__albumCommands.length===0`));
  await js(`window.fetch=window.__albumOriginalFetch;delete window.__albumOriginalFetch;delete window.__releaseAlbum;document.querySelector('.output-option[data-output="player:browser-local"]')?.click()`);
  await waitUntil(`document.querySelector('.output-trigger strong')?.textContent==='This Browser'`,2000);
}
