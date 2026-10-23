// Time's charts: drawn with ECharts from the JSON the server put in the page
// (#time-data). Numbers stay in tooltips (tap a mark); the page leads with
// pictures. Redrawn on theme change and on resize.
(function () {
  var el = document.getElementById('time-data');
  if (!el || !window.echarts) return;
  var D = JSON.parse(el.textContent);
  // One hue per activity, everywhere (same as the CSS palette).
  var C = {
    work: '#3987e5', study: '#199e70', comms: '#d55181', video: '#c98500',
    social: '#d95926', browsing: '#008300', music: '#9085e9', games: '#e66767',
    other: '#7d7c76', unknown: '#4a4a46'
  };
  var charts = [];

  function css(v) { return getComputedStyle(document.documentElement).getPropertyValue(v).trim(); }
  function dur(m) {
    m = Math.round(m);
    var h = Math.floor(m / 60), r = m % 60;
    return h ? (r ? h + ' h ' + r + ' min' : h + ' h') : r + ' min';
  }
  function hours(m) { return m >= 60 ? (m / 60).toFixed(m >= 600 ? 0 : 1) + 'h' : Math.round(m) + 'm'; }
  function thou(n) { return String(Math.round(n)).replace(/\B(?=(\d{3})+(?!\d))/g, ' '); }
  function pct(a, b) { return b ? (a >= b ? '+' : '−') + Math.round(Math.abs(a - b) * 100 / b) + '%' : ''; }
  function link(url) { if (url) location.href = url; }

  function make(id, opt) {
    var e = document.getElementById(id);
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

    // Where the time went: attention inside, a second device outside.
    var d = D.donut;
    make('c-donut', {
      animation: false,
      tooltip: Object.assign({ trigger: 'item', formatter: function (p) {
        var x = p.data;
        if (p.seriesIndex === 2) return '';
        if (x.gap) return '<b>' + x.with + '</b>: ' + dur(x.value) + ' with nothing else going on';
        if (p.seriesIndex === 1) return '<b>' + x.name + '</b> on a second device<br>while the attention was on ' + x.with + ': ' + dur(x.value);
        return '<b>' + x.name + '</b> ' + dur(x.value) + ' (' + p.percent + '%)<br>computer ' + dur(x.computer) + ' · phone ' + dur(x.phone);
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
            return Object.assign({}, x, x.gap
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

    // Over the day / week / month: minutes per activity, stacked.
    var t = D.time;
    var lines = [];
    if (D.kind !== 'day') {
      if (t.avg) lines.push({ yAxis: t.avg, name: 'average', lineStyle: { color: fg, type: 'dashed' }, label: { formatter: 'avg ' + hours(t.avg), color: fg, fontSize: 10, position: 'insideEndTop' } });
      if (t.prevAvg) lines.push({ yAxis: t.prevAvg, name: 'before', lineStyle: { color: mu, type: 'dotted' }, label: { formatter: 'before ' + hours(t.prevAvg), color: mu, fontSize: 10, position: 'insideStartTop' } });
    }
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
          markLine: i === 0 && lines.length ? { symbol: 'none', silent: true, data: lines } : undefined
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
        visualMap: { show: false, min: 0, max: maxw, dimension: 1, inRange: { color: [card, '#3987e5'] } },
        calendar: {
          orient: 'vertical', range: D.month, top: 22, left: 8, right: 8, bottom: 4, cellSize: ['auto', 'auto'],
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
          axisLabel: Object.assign({}, axisText, { interval: D.kind === 'day' ? 5 : (D.kind === 'month' ? 6 : 0) }) },
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
  }

  draw();
  window.addEventListener('timetheme', draw);
  window.addEventListener('resize', function () { charts.forEach(function (c) { c.resize(); }); });
})();
