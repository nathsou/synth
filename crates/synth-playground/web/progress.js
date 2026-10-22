// The progress of a run: the flow reports the step it is in (one of
// synth_core::progress::STEPS) and, when it can tell, how far through it is.
// Each family's steps come in its flow's order, weighted by their share of
// a run's time, so that the bar moves about evenly: measured on the
// examples, then learned from the runs in this browser (`learn`).

export const STEPS = {
  iCE40: [
    ['elaborate', 1], ['synthesize', 20], ['device', 1], ['pack', 1],
    ['place', 50], ['route', 22], ['timing', 1], ['bitstream', 4],
  ],
  'Artix-7': [
    ['elaborate', 1], ['synthesize', 25], ['pack', 1], ['device', 8],
    ['place', 5], ['route', 55], ['bitstream', 3], ['timing', 2],
  ],
};

export const LABELS = {
  elaborate: 'Elaborating',
  synthesize: 'Synthesising',
  device: 'Loading the device',
  pack: 'Packing',
  place: 'Placing',
  route: 'Routing',
  timing: 'Analysing timing',
  bitstream: 'Writing the bitstream',
};

/** The segments of the bar for progress `p` ({step, fraction, detail}, or
 *  null before the first report): each with its share of the bar and how
 *  full it is (null when the step cannot tell), and the whole run's
 *  fraction done. `learned` maps steps to weights that replace the
 *  defaults. */
export function layout(family, p, learned = {}) {
  const steps = (STEPS[family] || STEPS.iCE40).map(([s, w]) => [s, learned[s] ?? w]);
  const total = steps.reduce((a, [, w]) => a + w, 0);
  const at = p ? steps.findIndex(([s]) => s === p.step) : -1;
  let done = 0;
  const segments = steps.map(([step, w], i) => {
    let fill = 0;
    if (i < at) fill = 1;
    else if (i === at) fill = p.fraction ?? null;
    done += w * (fill ?? 0);
    return { step, label: LABELS[step] || step, share: w / total, fill };
  });
  return { segments, fraction: done / total, index: at, count: steps.length };
}

/** The status line for progress `p` after `ms` milliseconds. */
export function describe(p, ms) {
  const secs = ms < 10000 ? (ms / 1000).toFixed(1) : Math.round(ms / 1000);
  const what = p ? `${LABELS[p.step] || p.step}${p.detail ? ` · ${p.detail}` : ''}` : 'Starting';
  return `${what} · ${secs} s`;
}

/** Weights learned from one more run: `durations` maps the steps it went
 *  through to their milliseconds. Each run moves the weights a quarter of
 *  the way toward its own shares (in percent). */
export function learn(family, learned, durations) {
  const total = Object.values(durations).reduce((a, b) => a + b, 0);
  if (!(total > 0)) return learned;
  const out = {};
  for (const [s, w] of STEPS[family] || STEPS.iCE40) {
    const old = learned[s] ?? w;
    const share = 100 * (durations[s] || 0) / total;
    // Never quite zero: a step that takes no time today may tomorrow.
    out[s] = Math.max(0.5, 0.75 * old + 0.25 * share);
  }
  return out;
}
