// Run by `just hover-check` through check.mjs --eval. It times a hover over a job row on a fresh page,
// rebuilds the jobs table 200 times by flipping its phase filter, times the hover again, and throws if it
// slowed down. Rows that subscribe to a signal living as long as the page leave a dead subscriber behind
// on each rebuild, and every later write walks them all.
(async () => {
  const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
  const tick = () =>
    new Promise((done) => {
      const channel = new MessageChannel();
      channel.port1.onmessage = () => done();
      channel.port2.postMessage(0);
    });
  const rows = () => document.querySelectorAll(".panel.jobs tbody tr");
  const chip = (label) =>
    [...document.querySelectorAll(".panel.jobs .controls button")].find((button) => button.textContent.trim() === label);
  for (let waited = 0; rows().length < 1000 && waited < 60; waited++) await sleep(1000);
  const all = rows().length;
  if (all < 1000) throw new Error(`the jobs table holds ${all} rows, too few to show a slowdown`);
  const hover = async () => {
    const times = [];
    for (let i = 0; i < 21; i++) {
      const list = rows();
      const row = list[(i * 61) % list.length];
      const start = performance.now();
      row.dispatchEvent(new MouseEvent("mouseenter"));
      await tick();
      row.dispatchEvent(new MouseEvent("mouseleave"));
      await tick();
      times.push(performance.now() - start);
    }
    times.sort((a, b) => a - b);
    return times[10];
  };
  const fresh = await hover();
  let fewest = all;
  for (let i = 0; i < 100; i++) {
    chip("running").click();
    await tick();
    fewest = Math.min(fewest, rows().length);
    chip("all").click();
    await tick();
  }
  if (fewest >= all) throw new Error("flipping the phase filter did not rebuild the table");
  await sleep(500);
  const after = await hover();
  const result = { rows: all, rebuilds: 200, fresh_ms: Math.round(fresh * 10) / 10, after_ms: Math.round(after * 10) / 10 };
  if (after > 2 * fresh + 20) throw new Error(`a hover slowed down after the rebuilds: ${JSON.stringify(result)}`);
  return result;
})()
