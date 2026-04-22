const ws = new WebSocket('ws://127.0.0.1:8080/stream');
let n = 0; const imb = []; const total = [];
ws.addEventListener('message', e => {
  const t = JSON.parse(e.data);
  imb.push(t.imbalance);
  total.push((t.bid_depth||0) + (t.ask_depth||0));
  if (++n >= 150) {
    const stat = a => ({ min: Math.min(...a), max: Math.max(...a), range: Math.max(...a) - Math.min(...a) });
    console.log('IMB', stat(imb));
    console.log('TOTAL_DEPTH', stat(total));
    ws.close();
    process.exit(0);
  }
});
setTimeout(() => { console.error('TIMEOUT'); process.exit(2); }, 12000);
