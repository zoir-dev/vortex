import { invoke } from "@tauri-apps/api/core";

/**
 * Open the window wide enough that the UI never has to wrap.
 *
 * A fixed width in `tauri.conf.json` cannot do this: how wide the UI needs to
 * be depends on the display's scale factor, the UI font and the locale (the
 * Russian and Uzbek labels run materially wider than the English ones), none of
 * which are known when the config is written.
 *
 * The measurement is the whole trick, and the obvious ways to take it do not
 * work. `scrollWidth` on the document reports "fits" because the flex and grid
 * containers absorb the pressure instead of passing it up. Comparing each
 * element against its parent misses the same thing, because an element with
 * `overflow: visible` lets a too-wide child spill out in plain sight without
 * any of its own metrics changing. Both were tried against a live window whose
 * button was visibly hanging out of its card, and both reported zero.
 *
 * So the layout is asked directly instead: `html.vx-measuring` (see
 * `style.css`) forbids wrapping and lets the root size to its content, which
 * makes the UI state the width it actually wants. One read, class off, done.
 */

/** Width the UI needs in order not to wrap, in CSS pixels, plus the raw
 *  numbers behind it — reported to the log so a wrong answer can be told apart
 *  from a measurement that never happened. */
function measureNatural(): {
  natural: number;
  naturalHeight: number;
  appRect: number;
  appScroll: number;
  htmlScroll: number;
  bodyScroll: number;
  inner: number;
} {
  const root = document.getElementById("app");
  const html = document.documentElement;
  const inner = window.innerWidth;
  if (!root) {
    return {
      natural: 0,
      naturalHeight: 0,
      appRect: 0,
      appScroll: 0,
      htmlScroll: 0,
      bodyScroll: 0,
      inner,
    };
  }

  html.classList.add("vx-measuring");
  // Reading a layout property forces the reflow, so everything below is taken
  // against the measuring styles rather than the ones being replaced.
  void root.offsetWidth;

  const appRect = Math.ceil(root.getBoundingClientRect().width);
  const appScroll = root.scrollWidth;
  const htmlScroll = html.scrollWidth;
  const bodyScroll = document.body.scrollWidth;
  // Height has to be read HERE, with the measuring styles on, for the same
  // reason the width does: the page itself never scrolls — `<main>` is
  // `overflow-y-auto` and keeps the overflow to itself — so the document
  // reports no overflow at all and the window is never grown. Under
  // `overflow: visible` that inner scroll is released and the content pushes
  // the page to its true height.
  const naturalHeight = Math.max(
    html.scrollHeight,
    document.body.scrollHeight,
    root.scrollHeight,
  );

  html.classList.remove("vx-measuring");
  void root.offsetWidth;

  // The root keeps its normal width, so the controls that cannot wrap overflow
  // it instead — and with the flex and grid minimums freed (see `style.css`)
  // that overflow reaches the document, where `scrollWidth` reports it. Prose
  // is left wrapping, so this is the width the CONTROLS need and nothing more.
  const natural = Math.max(appScroll, htmlScroll, bodyScroll, appRect);
  return { natural, naturalHeight, appRect, appScroll, htmlScroll, bodyScroll, inner };
}

/**
 * Measure and, if the window is too small for the result, grow it.
 *
 * Grow-only: the measurement is what the content needs, not an opinion about
 * what the window should be, so it can widen a window that is too narrow but
 * never claw back one the user widened themselves.
 */
/**
 * Wait for the viewport to actually change after a resize, then settle.
 *
 * The backend resize is asynchronous — it returns as soon as the request is
 * made, not when the webview has been relaid out — so anything measured
 * immediately afterwards is still the OLD layout.
 */
/**
 * How much taller the content is than the box actually scrolling it.
 *
 * Measured in NORMAL rendering, not in measuring mode, and asked of the real
 * scrollers rather than the document. The page itself never scrolls here —
 * `<main>` is `overflow-y-auto`, so it keeps its overflow to itself and
 * `documentElement.scrollHeight` equals its client height no matter how much
 * content there is. Inferring the height from measuring mode instead
 * under-reported it and left the scrollbar exactly where it was.
 *
 * So: find every element that can scroll vertically, and take the largest gap
 * between what it holds and what it shows. That gap IS the scrollbar.
 */
function verticalDeficit(): number {
  let worst = 0;
  for (const el of Array.from(document.querySelectorAll<HTMLElement>("*"))) {
    const overflowY = getComputedStyle(el).overflowY;
    if (overflowY !== "auto" && overflowY !== "scroll") continue;
    const deficit = el.scrollHeight - el.clientHeight;
    if (deficit > worst) worst = deficit;
  }
  const doc = document.documentElement;
  return Math.max(worst, doc.scrollHeight - doc.clientHeight);
}

/** Let the engine finish laying out before anything is measured. Raced against
 *  a timer because WebKitGTK does not drive animation frames for a window that
 *  is not mapped, so the callback pair can simply never fire. */
function layoutSettled(): Promise<void> {
  const frames = new Promise<void>((r) =>
    requestAnimationFrame(() => requestAnimationFrame(() => r())),
  );
  return Promise.race([frames, new Promise<void>((r) => setTimeout(r, 300))]);
}

function waitForViewport(previousWidth: number, ms = 600): Promise<void> {
  return new Promise((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      window.removeEventListener("resize", onResize);
      resolve();
    };
    const onResize = () => {
      if (window.innerWidth !== previousWidth) finish();
    };
    window.addEventListener("resize", onResize);
    // The resize may never come (already the right size, or refused at the
    // monitor's limit); this is not a failure, just nothing to wait for.
    setTimeout(finish, ms);
  });
}

/**
 * Measure and, if the window is too small for the result, grow it.
 *
 * WIDTH FIRST, THEN HEIGHT — in two passes, with a relayout in between.
 *
 * The two are not independent: at a narrow width the content wraps into more
 * lines and stands taller, so a height measured before the widening describes a
 * layout that is about to stop existing. Measured together in one pass the
 * height came out 961px for a page that needs far less once the cards sit side
 * by side. So the width is applied first, the webview is allowed to reflow, and
 * only then is the height worth asking about.
 *
 * Grow-only in both directions: the measurement says what the content needs,
 * never what the window should be, so this can widen a cramped window but never
 * claw back one sized by hand.
 */
async function fitOnce(): Promise<boolean> {
  // Refuse to measure a layout that is not currently laid out.
  //
  // The webview reports a zero viewport while the window is being resized or
  // has not been mapped yet, and in that state the numbers are not small — they
  // are wrong. Caught live: `inner=0 app=2 appScroll=2220`, off which the
  // window was grown to 2220px.
  if (window.innerWidth < 200 || window.innerHeight < 200) return false;

  const first = measureNatural();
  if (first.natural < 200 || first.natural < window.innerWidth * 0.5) return false;

  let grew = false;

  // Phase 1 — width only. `heightRatio: 1` leaves the height alone, because
  // the height we could measure right now is the wrong one.
  if (first.natural > first.inner + 1) {
    const before = window.innerWidth;
    try {
      await invoke("fit_main_window", {
        widthRatio: first.natural / first.inner,
        heightRatio: 1,
        probe: `phase=width w=${first.natural} inner=${first.inner}`,
      });
      grew = true;
    } catch {
      return grew;
    }
    await waitForViewport(before);
    await layoutSettled();
    if (window.innerWidth < 200 || window.innerHeight < 200) return grew;
  }

  // Phase 2 — height, measured against the width the window now actually has,
  // and against the element that is actually doing the scrolling.
  const deficit = verticalDeficit();
  if (deficit > 1) {
    try {
      await invoke("fit_main_window", {
        widthRatio: 1,
        heightRatio: (window.innerHeight + deficit) / window.innerHeight,
        probe: `phase=height deficit=${deficit} inner=${window.innerWidth}x${window.innerHeight}`,
      });
      grew = true;
    } catch {
      // Non-fatal: costs the right size, never the window.
    }
  }

  return grew;
}

/** Coalesce bursts of DOM changes into one measurement. */
function debounce(fn: () => void, ms: number): () => void {
  let t: ReturnType<typeof setTimeout> | undefined;
  return () => {
    if (t !== undefined) clearTimeout(t);
    t = setTimeout(fn, ms);
  };
}

/**
 * Fit at boot, and again whenever the DOM changes enough to need it.
 *
 * Watching the DOM rather than measuring on a timer is the point. The widest
 * things in the UI — the device tiles and their buttons — only exist once peer
 * state has arrived over IPC, so any fixed delay is a guess about how long that
 * takes: too short and it measures an empty page (observed: the first pass
 * found nothing while a button was visibly overhanging), too long and the user
 * watches the window resize under them. A mutation IS the signal that something
 * appeared, so it is what triggers the re-measure.
 *
 * Safe to leave running for the session: every fit is grow-only, so this
 * converges and then costs one debounced measurement per DOM change.
 */
/** Set by [`initWindowFit`]; called by [`endWindowFit`]. */
let endFit: (() => void) | null = null;

/**
 * Stop fitting. Sizing belongs to the LANDING page only.
 *
 * Every fit is grow-only, so left running it would let each page in turn
 * redefine the window and never give the space back — open Settings once and
 * the window is as wide as the widest thing on it for the rest of the session.
 * Observed: Devices → Settings stretched the window to 3533px.
 *
 * Called from the router rather than a `hashchange` listener, which does not
 * fire: vue-router's `createWebHashHistory` navigates with `history.pushState`,
 * so in-app navigation changes the hash without ever raising that event. The
 * first version of this stop condition could therefore never trigger.
 */
export function endWindowFit(): void {
  endFit?.();
}

export function initWindowFit(): void {
  const root = document.getElementById("app");
  if (!root) return;

  let populated = false;
  let stopped = false;

  const observer = new MutationObserver(() => {
    populated = true;
    schedule();
  });

  function stop() {
    if (stopped) return;
    stopped = true;
    observer.disconnect();
  }
  endFit = stop;

  async function run() {
    if (stopped) return;
    const grew = await fitOnce();
    // Settled: the page has content and asked for nothing more. Sizing is a
    // startup job, and it is now done.
    if (!grew && populated) stop();
  }

  const schedule = debounce(() => void run(), 200);

  observer.observe(root, { childList: true, subtree: true, characterData: true });
  void run();
}
