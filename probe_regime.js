const ws = new WebSocket('ws://127.0.0.1:8080/stream');
let n = 0; const seen = {0:0,1:0,2:0,3:0};
let criticalAt = [];
ws.addEventListener('message', e => {
  const t = JSON.parse(e.data);
  seen[t.regime] = (seen[t.regime] || 0) + 1;
  if (t.regime === 3 && criticalAt.length < 10) criticalAt.push(n);
  if (++n >= 600) {
    console.log('DIST', JSON.stringify(seen));
    console.log('CRIT_AT', JSON.stringify(criticalAt));
    ws.close();
    process.exit(0);
  }
});
setTimeout(() => { console.error('TIMEOUT', JSON.stringify(seen)); process.exit(2); }, 25000);
