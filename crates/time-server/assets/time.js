// Time's charts: drawn with ECharts from the JSON the server put in the page
// (#time-data). Numbers stay in tooltips (tap a mark); the page leads with
// pictures. Redrawn on theme change and on resize.
(function () {
  var el = document.getElementById('time-data');
  if (!el || !window.echarts) return;
  var D = JSON.parse(el.textContent);
  // One hue per activity, everywhere (same as the CSS palette).
  var C = {
    work: '#3987e5', study: '#199e70', messaging: '#d55181', video: '#c98500',
    feeds: '#d95926', browsing: '#008300', music: '#9085e9', games: '#e66767',
    other: '#7d7c76', unknown: '#4a4a46', sleep: '#4651a6'
  };
  // Keyboard and mouse (one hue, darker for less), and voice: in is warm,
  // out is cool. Validated as one categorical set on the dark surface.
  var K = {
    typing: '#4fb3bf', pointer: '#2a6f78',
    dictation: '#c08a10', 'voice mode': '#1f9e8a', 'read aloud': '#5b7fe0', speak: '#d4567a',
    sleep: '#4651a6'
  };
  var charts = [];

  function css(v) { return getComputedStyle(document.documentElement).getPropertyValue(v).trim(); }
  function dur(m) {
    if (m > 0 && m < 1) return Math.max(1, Math.round(m * 60)) + ' s';
    m = Math.round(m);
    var h = Math.floor(m / 60), r = m % 60;
    return h ? (r ? h + ' h ' + r + ' min' : h + ' h') : r + ' min';
  }
  function hours(m) { return m >= 60 ? (m / 60).toFixed(m >= 600 ? 0 : 1) + 'h' : Math.round(m) + 'm'; }
  function thou(n) { return String(Math.round(n)).replace(/\B(?=(\d{3})+(?!\d))/g, ' '); }
  function pct(a, b) { return b ? (a >= b ? '+' : '−') + Math.round(Math.abs(a - b) * 100 / b) + '%' : ''; }
  function link(url) { if (url) location.href = url; }
  function esc(s) { return String(s).replace(/[&<>"]/g, function (c) { return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]; }); }
  // A breakdown for a tooltip: "x.com 60% · 34 min", the biggest first.
  function breakdown(top, n) {
    if (!top || !top.length) return '';
    var s = 0, out = '';
    top.forEach(function (x) { s += x.m; });
    top.slice(0, n).forEach(function (x) {
      out += '<br>' + esc(x.name) + ' <b>' + Math.round(x.m * 100 / (s || 1)) + '%</b> <span style="opacity:.7">' + dur(x.m) + '</span>';
    });
    out += '<br><i style="opacity:.7">' + (top.length > n ? (top.length - n) + ' more · ' : '') + 'tap to open</i>';
    return out;
  }

  function make(id, opt) { return makeIn(document.getElementById(id), opt); }
  function makeIn(e, opt) {
    if (!e) return null;
    var c = echarts.getInstanceByDom(e);
    if (c) c.dispose();
    c = echarts.init(e, null, { renderer: 'svg' });
    c.setOption(opt);
    charts.push(c);
    return c;
  }

  function draw() {
    charts.length = 0;
    var fg = css('--fg'), mu = css('--muted'), line = css('--line'), card = css('--card');
    var font = getComputedStyle(document.body).fontFamily;
    var tip = {
      confine: true, backgroundColor: card, borderColor: line,
      textStyle: { color: fg, fontSize: 12, fontFamily: font }
    };
    var axisText = { color: mu, fontSize: 10, fontFamily: font };
    var per = D.perDay ? ' a day' : '';
    // Sound sources: their own colours where they have one, else a cycle.
    var LC = ['#9085e9', '#3987e5', '#d55181', '#c98500', '#199e70', '#d95926'];
    var known = { spotify: '#1db954', 'youtube.com': '#e5484d', 'netflix.com': '#b81d24', 'music.youtube.com': '#ff6b6b' };
    var used = {};
    function lcol(key) {
      if (known[key]) return known[key];
      if (!key) return '#7d7c76';
      if (!used[key]) used[key] = LC[Object.keys(used).length % LC.length];
      return used[key];
    }
    function hhmm(min) { var h = Math.floor(min / 60), m = min % 60; return (h < 10 ? '0' : '') + h + ':' + (m < 10 ? '0' : '') + m; }

    function kcol(track, key) {
      if (track === 1 || track === 3) return lcol(key);
      if (track >= 4) return K[key] || '#7d7c76';
      return C[key] || C.other;
    }
    // A tapped mark opens its breakdown under the chart: bars, biggest first.
    function openDrill(id, title, rows, color) {
      var box = document.getElementById(id + '-drill');
      if (!box || !rows || !rows.length) return;
      box.hidden = false;
      box.querySelector('.dh b').textContent = title;
      box.querySelector('.dh button').onclick = function () { box.hidden = true; };
      var s = 0;
      rows.forEach(function (x) { s += x.m; });
      var r = rows.slice().reverse();
      var ch = box.querySelector('.chart');
      ch.style.height = (1.2 + r.length * 1.55) + 'rem';
      makeIn(ch, {
        animation: false,
        grid: { left: 4, right: 92, top: 2, bottom: 2, containLabel: true },
        tooltip: Object.assign({ trigger: 'item', formatter: function (p) {
          var x = r[p.dataIndex];
          return '<b>' + esc(x.name) + '</b> ' + dur(x.m) + ' (' + Math.round(x.m * 100 / (s || 1)) + '%)';
        } }, tip),
        xAxis: { type: 'value', show: false },
        yAxis: { type: 'category', axisTick: { show: false }, axisLine: { show: false },
          axisLabel: Object.assign({}, axisText, { color: fg, fontSize: 11, width: 170, overflow: 'truncate' }),
          data: r.map(function (x) { return x.name; }) },
        series: [{ type: 'bar', barWidth: 10,
          itemStyle: { color: color || mu, borderRadius: 3 },
          label: { show: true, position: 'right', color: mu, fontSize: 10, fontFamily: font,
            formatter: function (p) { var x = r[p.dataIndex]; return dur(x.m) + ' · ' + Math.round(x.m * 100 / (s || 1)) + '%'; } },
          data: r.map(function (x) { return { value: x.m, itemStyle: x.rest ? { color: line } : undefined }; }) }]
      });
      box.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    }

    // Rings: one per track, the full circle every minute anything was on.
    var R = D.rings;
    var RW = 0.092, RG = 0.014;
    function ringR(i) { var o = 0.95 - i * RW; return [o - RW + RG, o]; }
    if (R && R.full) {
      var days = R.days || 1;
      var rings = make('c-rings', {
        animation: false,
        tooltip: Object.assign({ trigger: 'item', formatter: function (p) {
          var x = p.data, tr = R.tracks[p.seriesIndex];
          if (!tr) return '';
          if (x.gap) return '<b>' + tr.name + '</b><br>nothing on this track ' + dur(x.value) + (D.perDay ? ' (' + dur(x.value / days) + ' a day)' : '');
          return '<b>' + tr.name + ' · ' + esc(x.name) + '</b><br>' + dur(x.value) + (D.perDay ? ' (' + dur(x.value / days) + ' a day)' : '') +
            ' · ' + Math.round(x.value * 100 / (tr.m || 1)) + '% of the track' + breakdown(x.top, 6);
        } }, tip),
        series: R.tracks.map(function (tr, i) {
          var used = 0;
          var data = tr.parts.map(function (p) {
            used += p.m;
            return { name: p.name, value: p.m, top: p.top, key: p.key, itemStyle: { color: kcol(i, p.key) } };
          });
          data.push({ name: '', value: Math.max(R.full - used, 0), gap: true,
            itemStyle: { color: line, opacity: 0.35 }, emphasis: { disabled: true } });
          var r = ringR(i);
          return { type: 'pie', radius: [r[0] * 100 + '%', r[1] * 100 + '%'], center: ['50%', '50%'], startAngle: 90,
            clockwise: true, label: { show: false }, labelLine: { show: false },
            itemStyle: { borderColor: card, borderWidth: 1 }, data: data };
        }).concat([{
          type: 'pie', radius: [0, '30%'], center: ['50%', '50%'], silent: true, tooltip: { show: false },
          label: { show: true, position: 'center', fontFamily: font,
            formatter: (function () {
              var pd = function (m) { return dur(D.perDay ? m / days : m); };
              var act = R.active != null ? R.active : R.full;
              var s = '{a|' + pd(act) + '}\n{b|' + (D.perDay ? 'a day with' : 'with') + ' anything on}';
              if (R.sleep) s += '\n{s|' + pd(R.sleep) + ' asleep}';
              if (R.untracked != null) s += '\n{b|' + pd(R.untracked) + ' untracked}';
              return s;
            })(),
            rich: { a: { color: fg, fontSize: 16, fontWeight: 700, lineHeight: 21 }, b: { color: mu, fontSize: 10, lineHeight: 13 },
              s: { color: '#8f98e8', fontSize: 10, fontWeight: 700, lineHeight: 14 } } },
          labelLine: { show: false }, data: [{ value: 1, name: '', itemStyle: { color: 'transparent' } }]
        }])
      });
      if (rings) {
        // Each ring's name and length where it starts, at 12 o'clock.
        var W = rings.getWidth(), H = rings.getHeight(), half = Math.min(W, H) / 2;
        rings.setOption({ graphic: R.tracks.map(function (tr, i) {
          var r = ringR(i), y = H / 2 - half * (r[0] + r[1]) / 2;
          return { type: 'text', silent: true, x: W / 2 - 6, y: y, style: {
            text: tr.name + '  ' + hours(D.perDay ? tr.m / days : tr.m), align: 'right', verticalAlign: 'middle',
            fill: tr.m ? fg : mu, fontSize: 10, fontFamily: font, backgroundColor: card, padding: [1, 3] } };
        }) });
        rings.on('click', function (p) {
          var tr = R.tracks[p.seriesIndex];
          if (tr && p.data && !p.data.gap) openDrill('c-rings', tr.name + ' · ' + p.data.name, p.data.top, kcol(p.seriesIndex, p.data.key));
        });
      }

      // The 24-hour clock: the same rings around a dial, midnight at the top.
      var K24 = D.clock;
      var clock = make('c-clock', { animation: false });
      if (clock) {
        var cw = clock.getWidth(), chh = clock.getHeight();
        var cx = cw / 2, cy = chh / 2, CR = Math.min(cw, chh) / 2 - 22;
        var ang = function (m) { return -Math.PI / 2 + m / 1440 * 2 * Math.PI; };
        var band = function (i) { var r = ringR(i); return [CR * r[0], CR * r[1]]; };
        var g = [];
        for (var i = 0; i < R.tracks.length; i++) {
          var b = band(i);
          g.push({ type: 'ring', silent: true, shape: { cx: cx, cy: cy, r: b[1], r0: b[0] }, style: { fill: line, opacity: 0.3 } });
        }
        for (var h = 0; h < 24; h++) {
          var a = ang(h * 60), major = h % 6 === 0, r1 = CR + 1, r2 = CR + (major ? 7 : 4);
          g.push({ type: 'line', silent: true, shape: { x1: cx + r1 * Math.cos(a), y1: cy + r1 * Math.sin(a), x2: cx + r2 * Math.cos(a), y2: cy + r2 * Math.sin(a) },
            style: { stroke: mu, lineWidth: major ? 1.5 : 1 } });
          if (h % 3 === 0) {
            g.push({ type: 'text', silent: true, x: cx + (CR + 15) * Math.cos(a), y: cy + (CR + 15) * Math.sin(a),
              style: { text: (h < 10 ? '0' : '') + h, align: 'center', verticalAlign: 'middle', fill: major ? fg : mu, fontSize: major ? 11 : 9, fontFamily: font, fontWeight: major ? 700 : 400 } });
          }
        }
        if (K24.now != null) {
          var an = ang(K24.now), r0 = CR * (ringR(R.tracks.length - 1)[0] - 0.03);
          g.push({ type: 'line', silent: true, z: 10, shape: { x1: cx + r0 * Math.cos(an), y1: cy + r0 * Math.sin(an), x2: cx + (CR + 4) * Math.cos(an), y2: cy + (CR + 4) * Math.sin(an) },
            style: { stroke: fg, lineWidth: 2 } });
          g.push({ type: 'circle', silent: true, z: 10, shape: { cx: cx + (CR + 4) * Math.cos(an), cy: cy + (CR + 4) * Math.sin(an), r: 3.5 }, style: { fill: fg } });
        }
        g.push({ type: 'text', silent: true, x: cx, y: cy, style: {
          text: (K24.now != null ? 'now ' + hhmm(K24.now) : (D.perDay ? '24 h' : 'the day')) + '\n' + (D.perDay ? 'on average' : ''),
          align: 'center', verticalAlign: 'middle', fill: fg, fontSize: 12, fontWeight: 700, fontFamily: font, lineHeight: 16 } });
        clock.setOption({
          tooltip: Object.assign({ trigger: 'item', formatter: function (p) {
            var x = p.data.value;
            return '<b>' + K24.names[x[0]] + '</b> · ' + hhmm(x[1]) + '–' + hhmm(Math.min(x[2], 1440)) +
              '<br>' + esc(x[4]) + (x[5] ? ' · ' + esc(x[5]) : '');
          } }, tip),
          graphic: g,
          series: [{
            type: 'custom', coordinateSystem: 'none',
            renderItem: function (params, api) {
              var x = K24.items[params.dataIndex], b = band(x[0]);
              return { type: 'sector', shape: { cx: cx, cy: cy, r0: b[0], r: b[1], startAngle: ang(x[1]),
                endAngle: ang(Math.max(x[2], x[1] + 1.5)), clockwise: true },
                style: { fill: kcol(x[0], x[3]), opacity: x[6] } };
            },
            data: K24.items.map(function (x) { return { value: x }; })
          }]
        });
        clock.on('click', function (p) {
          var x = p.data && p.data.value;
          if (!x) return;
          var tr = R.tracks[x[0]], part = tr && tr.parts.filter(function (q) { return q.key === x[3]; })[0];
          if (part) openDrill('c-clock', tr.name + ' · ' + part.name + (D.perDay ? '' : ' · the whole day'), part.top, kcol(x[0], x[3]));
        });
      }
    }

    // Where the time went: attention inside, a second device outside.
    var d = D.donut;
    var donut = make('c-donut', {
      animation: false,
      tooltip: Object.assign({ trigger: 'item', formatter: function (p) {
        var x = p.data;
        if (p.seriesIndex === 2) return '';
        if (x.gap) return '<b>' + x.with + '</b>: ' + dur(x.value) + ' with nothing else going on';
        if (p.seriesIndex === 1) return '<b>' + x.name + '</b> on a second device<br>while the attention was on ' + x.with + ': ' + dur(x.value);
        return '<b>' + x.name + '</b> ' + dur(x.value) + ' (' + p.percent + '%)<br>computer ' + dur(x.computer) + ' · phone ' + dur(x.phone) + breakdown((D.drill || {})[x.name], 6);
      } }, tip),
      legend: {
        bottom: 0, icon: 'circle', itemWidth: 9, itemHeight: 9, itemGap: 10,
        textStyle: { color: mu, fontSize: 11, fontFamily: font },
        data: d.inner.map(function (x) { return x.name; })
      },
      series: [
        {
          type: 'pie', radius: ['44%', '70%'], center: ['50%', '45%'], startAngle: 90,
          label: { show: true, position: 'inside', color: '#141413', fontSize: 10, fontWeight: 700, fontFamily: font,
            formatter: function (p) { return p.percent >= 8 ? Math.round(p.percent) + '%' : ''; } },
          labelLine: { show: false }, itemStyle: { borderColor: card, borderWidth: 1.5 },
          data: d.inner.map(function (x) { return Object.assign({}, x, { itemStyle: { color: C[x.name] } }); })
        },
        {
          type: 'pie', radius: ['74%', '83%'], center: ['50%', '45%'], startAngle: 90,
          label: { show: false }, labelLine: { show: false }, itemStyle: { borderColor: card, borderWidth: 1 },
          data: d.outer.map(function (x) {
            // Slivers under 0.2% of the period read as noise: grey.
            var tiny = !x.gap && x.value < d.total / 500;
            return Object.assign({}, x, x.gap || tiny
              ? { itemStyle: { color: line, opacity: 0.35 }, emphasis: { disabled: true } }
              : { itemStyle: { color: C[x.name] } });
          })
        },
        {
          type: 'pie', radius: [0, '40%'], center: ['50%', '45%'], silent: true, tooltip: { show: false },
          label: { show: true, position: 'center', fontFamily: font,
            formatter: '{a|' + dur(d.total) + '}\n{b|' + (d.parallel ? dur(d.parallel) + ' in parallel' : 'tracked') + '}',
            rich: { a: { color: fg, fontSize: 17, fontWeight: 700, lineHeight: 24 }, b: { color: mu, fontSize: 11 } } },
          labelLine: { show: false }, data: [{ value: 1, name: '', itemStyle: { color: 'transparent' } }]
        }
      ]
    });
    if (donut) donut.on('click', function (p) {
      if (p.seriesIndex === 0) openDrill('c-donut', p.data.name + ' · what filled it', (D.drill || {})[p.data.name], C[p.data.name]);
    });

    // In parallel: the biggest pairs, each bar half one activity, half the other.
    var pairs = (D.pairs || []).slice().reverse();
    make('c-pairs', {
      animation: false,
      grid: { left: 4, right: 44, top: 4, bottom: 4, containLabel: true },
      tooltip: Object.assign({ trigger: 'item', formatter: function (p) {
        var x = pairs[p.dataIndex];
        return '<b>' + x.a + '</b> on the ' + x.aOn + '<br>+ <b>' + x.b + '</b> on the ' + x.bOn + '<br>' + dur(x.m);
      } }, tip),
      xAxis: { type: 'value', show: false },
      yAxis: { type: 'category', axisLine: { show: false }, axisTick: { show: false },
        axisLabel: Object.assign({}, axisText, { fontSize: 11, color: fg }),
        data: pairs.map(function (x) { return x.a + ' + ' + x.b + (x.bOn === 'phone' ? ' 📱' : ' 💻'); }) },
      series: [{
        type: 'bar', barWidth: 12,
        label: { show: true, position: 'right', color: mu, fontSize: 11, fontFamily: font,
          formatter: function (p) { return hours(p.value); } },
        data: pairs.map(function (x) {
          return { value: x.m, itemStyle: { borderRadius: 4, color: { type: 'linear', x: 0, y: 0, x2: 1, y2: 0, colorStops: [
            { offset: 0, color: C[x.a] }, { offset: 0.5, color: C[x.a] }, { offset: 0.5, color: C[x.b] }, { offset: 1, color: C[x.b] }] } } };
        })
      }]
    });

    // Six tracks over time: lanes for a day, a bar per track per day otherwise.
    var T = D.tracks;
    if (T && T.any) {
      if (D.kind === 'day') {
        // Lanes: every stretch minute-exact; keyboard and mouse per ten
        // minutes, darker for more of it with input.
        var names = T.names, items = D.clock.items;
        make('c-tracks', {
          animation: false,
          grid: { left: 4, right: 10, top: 4, bottom: 4, containLabel: true },
          tooltip: Object.assign({ trigger: 'item', formatter: function (p) {
            var s = p.value;
            return '<b>' + names[s[0]] + '</b> · ' + hhmm(s[1]) + '–' + hhmm(Math.min(s[2], 1440)) + ' (' + dur(s[2] - s[1]) + ')<br>' +
              esc(s[4]) + (s[5] ? ' · ' + esc(s[5]) : '');
          } }, tip),
          xAxis: { type: 'value', min: 0, max: T.minutes, interval: 180, splitLine: { lineStyle: { color: line, opacity: 0.6 } },
            axisLabel: Object.assign({}, axisText, { formatter: function (v) { return hhmm(v).slice(0, 2); } }) },
          yAxis: { type: 'category', inverse: true, data: names, axisTick: { show: false }, axisLine: { show: false },
            axisLabel: Object.assign({}, axisText, { color: fg, fontSize: 11 }) },
          series: [{
            type: 'custom',
            renderItem: function (params, api) {
              var a = api.coord([api.value(1), api.value(0)]), b = api.coord([api.value(2), api.value(0)]);
              var h = api.size([0, 1])[1] * 0.62, x = items[params.dataIndex];
              return { type: 'rect', shape: { x: a[0], y: a[1] - h / 2, width: Math.max(b[0] - a[0], 1.5), height: h },
                style: { fill: kcol(x[0], x[3]), opacity: x[6] } };
            },
            encode: { x: [1, 2], y: 0 },
            data: items
          }]
        });
      } else {
        var TC = ['#3987e5', '#9085e9', '#d95926', '#1db954', '#4fb3bf', '#c08a10', '#4651a6'];
        var tr = make('c-tracks', {
          animation: false,
          grid: { left: 4, right: 4, top: 40, bottom: 4, containLabel: true },
          legend: { top: 0, itemWidth: 10, itemHeight: 8, textStyle: { color: mu, fontSize: 10, fontFamily: font } },
          tooltip: Object.assign({ trigger: 'axis', axisPointer: { type: 'shadow' }, formatter: function (ps) {
            var out = '<b>' + ps[0].axisValue + '</b>';
            ps.forEach(function (p) { out += '<br><span style="color:' + p.color + '">●</span> ' + p.seriesName + ' ' + dur(p.value); });
            return out;
          } }, tip),
          xAxis: { type: 'category', data: T.labels, axisTick: { show: false }, axisLine: { lineStyle: { color: line } },
            axisLabel: Object.assign({}, axisText, { interval: D.kind === 'month' ? 4 : 0 }) },
          yAxis: { type: 'value', splitNumber: 3, splitLine: { lineStyle: { color: line, opacity: 0.6 } },
            axisLabel: Object.assign({}, axisText, { formatter: function (v) { return hours(v); } }) },
          series: T.names.map(function (n, i) {
            return { type: 'bar', name: n, barGap: '10%', barCategoryGap: '25%', itemStyle: { color: TC[i], borderRadius: [2, 2, 0, 0] }, data: T.perDay[i] };
          })
        });
        if (tr && D.time.links.length) tr.on('click', function (p) { link(D.time.links[p.dataIndex]); });
      }
    }
    // Over the day / week / month: minutes per activity, stacked.
    var t = D.time;
    var lines = [];
    if (D.kind !== 'day') {
      // The caption above the chart names the lines; no labels in the plot.
      if (t.avg) lines.push({ yAxis: t.avg, name: 'average', lineStyle: { color: fg, type: 'dashed', width: 1.5 }, label: { show: false } });
      if (t.prevAvg) lines.push({ yAxis: t.prevAvg, name: 'before', lineStyle: { color: mu, type: 'dotted', width: 1.5 }, label: { show: false } });
    }
    // Days still to come are shaded, not mistaken for empty days.
    var first = t.future.indexOf(true);
    var futureArea = first < 0 ? undefined : { silent: true, itemStyle: { color: line, opacity: 0.35 },
      data: [[{ xAxis: t.labels[first] }, { xAxis: t.labels[t.labels.length - 1] }]] };
    var time = make('c-time', {
      animation: false,
      grid: { left: 4, right: 4, top: 14, bottom: 4, containLabel: true },
      tooltip: Object.assign({ trigger: 'axis', axisPointer: { type: 'shadow' }, formatter: function (ps) {
        var total = 0, rows = '';
        ps.forEach(function (p) { if (p.value) { total += p.value; rows += '<br><span style="color:' + p.color + '">●</span> ' + p.seriesName + ' ' + dur(p.value); } });
        var head = D.kind === 'day' ? ps[0].axisValue + ':00' : ps[0].axisValue;
        return '<b>' + head + '</b> · ' + dur(total) + rows + (D.kind !== 'day' ? '<br><i>tap to open the day</i>' : '');
      } }, tip),
      xAxis: { type: 'category', data: t.labels, axisTick: { show: false }, axisLine: { lineStyle: { color: line } },
        axisLabel: Object.assign({}, axisText, { interval: D.kind === 'day' ? 5 : (D.kind === 'month' ? 4 : 0) }) },
      yAxis: { type: 'value', splitNumber: 3, splitLine: { lineStyle: { color: line, opacity: 0.6 } },
        axisLabel: Object.assign({}, axisText, { formatter: function (v) { return D.kind === 'day' ? v + 'm' : hours(v); } }),
        max: D.kind === 'day' ? 60 : null },
      series: t.series.map(function (s, i) {
        return {
          type: 'bar', name: s.name, stack: 'a', barCategoryGap: D.kind === 'month' ? '20%' : '30%',
          itemStyle: { color: C[s.name] }, data: s.data,
          markLine: i === 0 && lines.length ? { symbol: 'none', silent: true, data: lines } : undefined,
          markArea: i === 0 ? futureArea : undefined
        };
      })
    });
    if (time && t.links.length) time.on('click', function (p) { link(t.links[p.dataIndex]); });

    // The month as a calendar of work and study.
    if (D.kind === 'month' && D.calendar.length) {
      var maxw = Math.max.apply(null, D.calendar.map(function (x) { return x[1]; }).concat([60]));
      var cal = make('c-cal', {
        animation: false,
        tooltip: Object.assign({ trigger: 'item', formatter: function (p) {
          var x = p.data;
          return '<b>' + x[0] + '</b><br>work and study ' + dur(x[1]) + '<br>tracked ' + dur(x[2]) + (x[3] ? '<br>' + thou(x[3]) + ' steps' : '');
        } }, tip),
        visualMap: { show: true, type: 'continuous', orient: 'horizontal', left: 'center', bottom: 0,
          itemWidth: 8, itemHeight: 120, calculable: false, min: 0, max: maxw, dimension: 1,
          text: [hours(maxw), '0h'], textStyle: { color: mu, fontSize: 10 },
          inRange: { color: [card, '#3987e5'] } },
        calendar: {
          orient: 'vertical', range: D.month, top: 22, left: 8, right: 8, bottom: 34, cellSize: ['auto', 'auto'],
          splitLine: { show: false }, itemStyle: { color: card, borderColor: line, borderWidth: 2 },
          dayLabel: { firstDay: 1, nameMap: ['S', 'M', 'T', 'W', 'T', 'F', 'S'], color: mu, fontSize: 10, margin: 6 },
          monthLabel: { show: false }, yearLabel: { show: false }
        },
        series: [{
          type: 'heatmap', coordinateSystem: 'calendar', data: D.calendar,
          label: { show: true, color: fg, fontSize: 10, fontFamily: font,
            formatter: function (p) { var day = +p.data[0].slice(8); return day + (p.data[1] >= 30 ? '\n' + hours(p.data[1]) : ''); } }
        }]
      });
      if (cal) cal.on('click', function (p) { link('/day/' + p.data[0]); });
    }

    // Steps: a goal ring and bars against the goal.
    var s = D.steps;
    if (s.any) {
      var done = s.now >= s.goal;
      make('c-ring', {
        animation: false,
        series: [{
          type: 'gauge', startAngle: 90, endAngle: -270, min: 0, max: s.goal, radius: '92%',
          pointer: { show: false }, axisLine: { lineStyle: { width: 11, color: [[1, line]] } },
          progress: { show: true, roundCap: true, width: 11, itemStyle: { color: done ? css('--ok') : '#3987e5' } },
          splitLine: { show: false }, axisTick: { show: false }, axisLabel: { show: false },
          title: { show: false },
          detail: { offsetCenter: [0, 0], fontFamily: font, formatter: function () {
            return '{a|' + thou(s.now) + '}\n{b|steps' + per + '}\n{b|' + s.km.toFixed(1) + ' km}' + (s.prev ? '\n{c|' + pct(s.now, s.prev) + '}' : '');
          }, rich: { a: { color: fg, fontSize: 15, fontWeight: 700, lineHeight: 20 }, b: { color: mu, fontSize: 10, lineHeight: 13 }, c: { color: mu, fontSize: 10, lineHeight: 14 } } },
          data: [{ value: Math.min(s.now, s.goal) }]
        }]
      });
      make('c-steps', {
        animation: false,
        grid: { left: 2, right: 2, top: 8, bottom: 2, containLabel: true },
        tooltip: Object.assign({ trigger: 'axis', axisPointer: { type: 'shadow' }, formatter: function (ps) {
          return '<b>' + ps[0].axisValue + (D.kind === 'day' ? ':00' : '') + '</b><br>' + thou(ps[0].value) + ' steps';
        } }, tip),
        xAxis: { type: 'category', data: s.labels, axisTick: { show: false }, axisLine: { lineStyle: { color: line } },
          axisLabel: Object.assign({}, axisText, { interval: D.kind === 'day' ? 5 : (D.kind === 'month' ? 6 : 0),
            formatter: function (v) { return D.kind === 'week' ? v.charAt(0) : v; } }) },
        yAxis: { type: 'value', show: false, max: function (v) { return Math.max(v.max, D.kind === 'day' ? 0 : s.goal); } },
        series: [{
          type: 'bar', barCategoryGap: '25%',
          data: s.values.map(function (v) { return { value: v, itemStyle: { borderRadius: [3, 3, 0, 0], color: D.kind !== 'day' && v >= s.goal ? css('--ok') : '#3987e5' } }; }),
          markLine: D.kind === 'day' ? undefined : { symbol: 'none', silent: true, data: [{ yAxis: s.goal, lineStyle: { color: css('--ok'), type: 'dashed' }, label: { formatter: 'goal', color: mu, fontSize: 10, position: 'insideEndTop' } }] }
        }]
      });
    }

    // This period against the one before, per activity.
    var cmp = (D.compare || []).slice(0, 8).reverse();
    make('c-compare', {
      animation: false,
      grid: { left: 4, right: 40, top: 22, bottom: 2, containLabel: true },
      legend: { top: 0, right: 0, itemWidth: 12, itemHeight: 8, textStyle: { color: mu, fontSize: 10, fontFamily: font },
        data: ['now' + per, 'before' + per] },
      tooltip: Object.assign({ trigger: 'axis', axisPointer: { type: 'shadow' }, formatter: function (ps) {
        var x = cmp[ps[0].dataIndex];
        return '<b>' + x.name + '</b><br>now ' + dur(x.cur) + per + ' (' + pct(x.cur, x.prev) + ')<br>before ' + dur(x.prev) + per + (D.perDay ? '<br>' + dur(x.total) + ' in total' : '');
      } }, tip),
      xAxis: { type: 'value', show: false },
      yAxis: { type: 'category', axisTick: { show: false }, axisLine: { show: false },
        axisLabel: Object.assign({}, axisText, { color: fg, fontSize: 11 }), data: cmp.map(function (x) { return x.name; }) },
      series: [
        { name: 'before' + per, type: 'bar', barWidth: 6, barGap: '15%', itemStyle: { color: mu, opacity: 0.45, borderRadius: 3 },
          data: cmp.map(function (x) { return x.prev; }) },
        { name: 'now' + per, type: 'bar', barWidth: 9, itemStyle: { borderRadius: 3 },
          label: { show: true, position: 'right', color: mu, fontSize: 10, fontFamily: font,
            formatter: function (p) { var x = cmp[p.dataIndex]; return x.prev ? pct(x.cur, x.prev) : ''; } },
          data: cmp.map(function (x) { return { value: x.cur, itemStyle: { color: C[x.name] } }; }) }
      ]
    });

    // Listening: what could really be heard, per source, then when.
    var L = D.listening;
    if (L && L.total) {
      var src = L.sources.slice().reverse();
      make('c-listen', {
        animation: false,
        grid: { left: 4, right: 64, top: 2, bottom: 2, containLabel: true },
        tooltip: Object.assign({ trigger: 'axis', axisPointer: { type: 'shadow' }, formatter: function (ps) {
          var x = src[ps[0].dataIndex];
          return '<b>' + x.name + '</b> ' + dur(x.m) + (x.away ? '<br>at a device ' + dur(x.m - x.away) + '<br>away from the keyboard ' + dur(x.away) : '');
        } }, tip),
        xAxis: { type: 'value', show: false },
        yAxis: { type: 'category', axisTick: { show: false }, axisLine: { show: false },
          axisLabel: Object.assign({}, axisText, { color: fg, fontSize: 11 }), data: src.map(function (x) { return x.name; }) },
        series: [
          { type: 'bar', stack: 'l', barWidth: 12,
            data: src.map(function (x) { return { value: x.m - x.away, itemStyle: { color: lcol(x.key), borderRadius: x.away ? [4, 0, 0, 4] : 4 } }; }) },
          { type: 'bar', stack: 'l', barWidth: 12,
            label: { show: true, position: 'right', color: mu, fontSize: 11, fontFamily: font,
              formatter: function (p) { return dur(src[p.dataIndex].m); } },
            data: src.map(function (x) { return { value: x.away, itemStyle: { color: lcol(x.key), opacity: 0.4, borderRadius: x.m - x.away ? [0, 4, 4, 0] : 4 } }; }) }
        ]
      });
      var lt = make('c-listen-time', {
        animation: false,
        grid: { left: 4, right: 4, top: 10, bottom: 4, containLabel: true },
        tooltip: Object.assign({ trigger: 'axis', axisPointer: { type: 'shadow' }, formatter: function (ps) {
          var total = 0, rows = '';
          ps.forEach(function (p) { if (p.value) { total += p.value; rows += '<br><span style="color:' + p.color + '">●</span> ' + p.seriesName + ' ' + dur(p.value); } });
          return '<b>' + ps[0].axisValue + (D.kind === 'day' ? ':00' : '') + '</b> · ' + dur(total) + rows;
        } }, tip),
        xAxis: { type: 'category', data: L.labels, axisTick: { show: false }, axisLine: { lineStyle: { color: line } },
          axisLabel: Object.assign({}, axisText, { interval: D.kind === 'day' ? 5 : (D.kind === 'month' ? 4 : 0) }) },
        yAxis: { type: 'value', splitNumber: 2, splitLine: { lineStyle: { color: line, opacity: 0.6 } },
          axisLabel: Object.assign({}, axisText, { formatter: function (v) { return D.kind === 'day' ? v + 'm' : hours(v); } }),
          max: D.kind === 'day' ? 60 : null },
        series: L.series.map(function (s) {
          return { type: 'bar', name: s.name, stack: 'l', barCategoryGap: '30%', itemStyle: { color: lcol(s.key) }, data: s.data };
        })
      });
      if (lt && D.time.links.length) lt.on('click', function (p) { link(D.time.links[p.dataIndex]); });
    }
    // Projects.
    var pr = (D.projects || []).slice().reverse();
    make('c-projects', {
      animation: false,
      grid: { left: 4, right: 44, top: 2, bottom: 2, containLabel: true },
      tooltip: Object.assign({ trigger: 'item', formatter: function (p) { return '<b>' + pr[p.dataIndex].name + '</b> ' + dur(p.value); } }, tip),
      xAxis: { type: 'value', show: false },
      yAxis: { type: 'category', axisTick: { show: false }, axisLine: { show: false },
        axisLabel: Object.assign({}, axisText, { color: fg, fontSize: 11 }), data: pr.map(function (x) { return x.name; }) },
      series: [{ type: 'bar', barWidth: 10, itemStyle: { color: C.work, borderRadius: 3 },
        label: { show: true, position: 'right', color: mu, fontSize: 10, fontFamily: font, formatter: function (p) { return hours(p.value); } },
        data: pr.map(function (x) { return x.m; }) }]
    });
    // Code: commits per repository.
    var cd = D.code && D.code.repos ? D.code.repos.slice().reverse() : [];
    make('c-code', cd.length ? {
      animation: false,
      grid: { left: 4, right: 30, top: 2, bottom: 2, containLabel: true },
      tooltip: Object.assign({ trigger: 'item', formatter: function (p) { return '<b>' + esc(cd[p.dataIndex].name) + '</b> ' + p.value + ' commits'; } }, tip),
      xAxis: { type: 'value', show: false },
      yAxis: { type: 'category', axisTick: { show: false }, axisLine: { show: false },
        axisLabel: Object.assign({}, axisText, { color: fg, fontSize: 11 }), data: cd.map(function (x) { return x.name; }) },
      series: [{ type: 'bar', barWidth: 10, itemStyle: { color: '#199e70', borderRadius: 3 },
        label: { show: true, position: 'right', color: mu, fontSize: 10, fontFamily: font },
        data: cd.map(function (x) { return x.n; }) }]
    } : { graphic: [{ type: 'text', left: 'center', top: 'middle', style: { text: 'no commits', fill: mu, fontSize: 12, fontFamily: font } }] });
  }

  draw();
  window.addEventListener('timetheme', draw);
  // The rings' and the clock's labels are placed in pixels: redraw, not resize.
  var redraw = null;
  window.addEventListener('resize', function () { clearTimeout(redraw); redraw = setTimeout(draw, 150); });
})();
