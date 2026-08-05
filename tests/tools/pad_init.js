
window.__pad = {
  index: 0, id: "Selkies Test Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)",
  mapping: "standard", connected: true, timestamp: 1,
  buttons: Array.from({length: 17}, () => ({pressed: false, touched: false, value: 0})),
  axes: [0, 0, 0, 0],
};
window.__gpCalls = 0;
navigator.getGamepads = () => { window.__gpCalls++; return [window.__pad, null, null, null]; };
window.__padPress = (i, v) => {
  window.__pad.buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
  window.__pad.timestamp = performance.now();
};
window.__padAxis = (i, v) => {
  window.__pad.axes[i] = v;
  window.__pad.timestamp = performance.now();
};

window.__timers = {};
(() => {
  const SI = window.setInterval, CI = window.clearInterval;
  window.setInterval = function(fn, ms, ...rest) {
    const id = SI.call(window, function() { window.__timers[id] = (window.__timers[id]||0) + 1; return fn.apply(this, arguments); }, ms, ...rest);
    window.__timers['created_' + id] = ms;
    return id;
  };
  window.clearInterval = function(id) { window.__timers['cleared_' + id] = (window.__timers['cleared_' + id]||0)+1; return CI.call(window, id); };
})();
