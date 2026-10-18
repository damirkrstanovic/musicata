// Output destinations are selectors only: choosing one changes the local control target, never
// rewrites zone membership, Snapcast routing, or a queue. The caller wires this into v2-flows
// after its disposable player/zone fixture has been created.
export async function outputPickerFlows({js, check, waitUntil, tap, groupedPlayerId}) {
  const settle = () => new Promise(resolve => setTimeout(resolve, 250));
  await tap('.output-trigger');
  await waitUntil(`document.querySelector('.output-picker:popover-open')`, 2000);

  check('Output picker: a zoned player is nested instead of duplicated as a standalone destination', await js(`
    !document.querySelector('.output-option[data-output="player:${groupedPlayerId}"]') &&
    !!document.querySelector('.output-option[data-output^="zone:"]')
  `));

  // Selection may reconnect the state WebSocket and persist the local preference. Neither is a
  // server-side mutation. Keep the original fetch so this only observes calls made by the tap.
  await js(`(() => {
    window.__outputPickerRequests = [];
    window.__outputPickerFetch = window.fetch;
    window.fetch = async (...args) => {
      const init = args[1] || {};
      window.__outputPickerRequests.push({url: String(args[0]), method: init.method || 'GET'});
      return window.__outputPickerFetch(...args);
    };
  })()`);
  const chosen = await tap('.output-option[data-output^="zone:"]', true);
  await settle();
  const mutations = await js(`window.__outputPickerRequests.filter(request => request.method !== 'GET')`);
  await js(`window.fetch = window.__outputPickerFetch; delete window.__outputPickerFetch`);

  check('Output picker: destination selection is reachable', chosen);
  check('Output picker: destination selection makes no membership, routing, or queue mutation', mutations.length === 0, JSON.stringify(mutations));
}
