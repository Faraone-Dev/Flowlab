const ws = new WebSocket('ws://127.0.0.1:8080/stream');
let n = 0; const seen = {0:0,1:0,2:0,3:0};
const mids = []; const imbs = []; const dDeltas = [];
ws.addEventListener('message', e => {
  const t = JSON.parse(e.data);
  seen[t.regime] = (seen[t.regime]||0)+1;
  mids.push(t.mid_ticks);
  imbs.push(t.imbalance);
  dDeltas.push((t.bid_depth||0) - (t.ask_depth||0));
  if (++n >= 200) {
    const stat = a => { const mn=Math.min(...a), mx=Math.max(...a); return {min:mn, max:mx, range: mx-mn}; };
    console.log('REGIME', JSON.stringify(seen));
    console.log('MID    ', stat(mids));
    console.log('IMB    ', stat(imbs));
    console.log('DEPTH  ', stat(dDeltas));
    ws.close(); process.exit(0);
  }
});
ws.addEventListener('error', e => { console.error('ERR', e.message||e); process.exit(1); });
setTimeout(() => { console.error('TIMEOUT', n); process.exit(2); }, 20000);
