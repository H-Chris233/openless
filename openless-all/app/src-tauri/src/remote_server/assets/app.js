/* ============================================================
 * OpenLess 远程输入 — 手机端录音页
 * 纯静态,无外部依赖。通过 WSS 把 16kHz/单声道/16bit LE PCM
 * 实时推送给 PC 端 Rust 服务。
 * ========================================================== */
(function () {
  'use strict';

  // ---------- 常量 ----------
  var TARGET_SR = 16000;            // 目标采样率,必须与 PC 端一致
  var MODE_KEY = 'ol_remote_mode';  // localStorage 键

  // ---------- DOM ----------
  var $ = function (id) { return document.getElementById(id); };
  var screenPin = $('screen-pin');
  var screenRec = $('screen-rec');
  var screenOffline = $('screen-offline');

  var pinInput = $('pin-input');
  var pinError = $('pin-error');
  var btnConnect = $('btn-connect');

  var recordBtn = $('btn-record');
  var recordLabel = $('record-label');
  var statusBar = $('status-bar');
  var statusText = $('status-text');
  var levelBar = $('level-bar');
  var recTip = $('rec-tip');
  var connDot = $('conn-dot');
  var modeSwitch = $('mode-switch');

  var btnReconnect = $('btn-reconnect');
  var offlineReason = $('offline-reason');

  // ---------- 状态 ----------
  var ws = null;
  var authed = false;
  var recording = false;          // 是否正在录音(决定是否 send 音频)
  var busy = false;               // PC 端忙,本次禁用
  var mode = readMode();          // 'toggle' | 'hold'
  var lastPin = '';

  // 音频相关
  var audioCtx = null;
  var mediaStream = null;
  var sourceNode = null;
  var workletNode = null;
  var scriptNode = null;
  var workletUrl = null;
  var usingWorklet = false;
  // ScriptProcessor 兜底用的重采样状态(跨块保留)
  var resampleState = { phase: 0, last: 0, hasLast: false };

  // ============================================================
  // 屏幕切换
  // ============================================================
  function showScreen(which) {
    screenPin.classList.toggle('active', which === 'pin');
    screenRec.classList.toggle('active', which === 'rec');
    screenOffline.classList.toggle('active', which === 'offline');
  }

  // ============================================================
  // 模式(toggle / hold)
  // ============================================================
  function readMode() {
    var m = null;
    try { m = localStorage.getItem(MODE_KEY); } catch (e) {}
    return m === 'hold' ? 'hold' : 'toggle';
  }
  function writeMode(m) {
    mode = m;
    try { localStorage.setItem(MODE_KEY, m); } catch (e) {}
    syncModeUI();
  }
  function syncModeUI() {
    var btns = modeSwitch.querySelectorAll('.mode-btn');
    for (var i = 0; i < btns.length; i++) {
      btns[i].classList.toggle('active', btns[i].getAttribute('data-mode') === mode);
    }
    if (mode === 'hold') {
      recTip.textContent = '按住大按钮说话,松开结束并识别。';
      recordLabel.textContent = recording ? '松开结束' : '按住说话';
      recordBtn.style.touchAction = 'none';   // hold 防滚动
    } else {
      recTip.textContent = '点击大按钮开始录音,再次点击结束并识别。';
      recordLabel.textContent = recording ? '点击结束' : '点击开始';
      recordBtn.style.touchAction = 'manipulation';
    }
  }

  // 切换模式时若约定的 prefer 变化,告知 PC(若已连接)
  modeSwitch.addEventListener('click', function (e) {
    var t = e.target.closest('.mode-btn');
    if (!t) return;
    var m = t.getAttribute('data-mode');
    if (m === mode) return;
    // 录音中切换模式先安全停止(取消本次,避免状态错乱)
    if (recording) cancelRecording();
    writeMode(m);
  });

  // ============================================================
  // 状态文字 / 音量
  // ============================================================
  function setStatus(text, kind) {
    statusText.textContent = text;
    statusBar.classList.remove('is-error', 'is-ok', 'is-work');
    if (kind === 'error') statusBar.classList.add('is-error');
    else if (kind === 'ok') statusBar.classList.add('is-ok');
    else if (kind === 'work') statusBar.classList.add('is-work');
  }
  function setLevel(v) {
    if (typeof v !== 'number' || isNaN(v)) return;
    v = Math.max(0, Math.min(1, v));
    levelBar.style.width = (v * 100).toFixed(1) + '%';
  }

  // ============================================================
  // WebSocket
  // ============================================================
  function wsSendJSON(obj) {
    if (ws && ws.readyState === 1) {
      try { ws.send(JSON.stringify(obj)); } catch (e) {}
    }
  }

  function connect(pin) {
    lastPin = pin;
    closeWS(); // 清理旧连接
    authed = false;
    busy = false;

    var url = 'wss://' + location.host + '/ws';
    try {
      ws = new WebSocket(url);
    } catch (e) {
      showPinError('无法建立连接,请检查网络。');
      resetConnectBtn();
      return;
    }
    ws.binaryType = 'arraybuffer';

    ws.onopen = function () {
      // 连上立即握手
      wsSendJSON({ type: 'hello', pin: pin, prefer: mode });
    };

    ws.onmessage = function (ev) {
      if (typeof ev.data !== 'string') return; // 下行只处理文本
      var msg;
      try { msg = JSON.parse(ev.data); } catch (e) { return; }
      handleMessage(msg);
    };

    ws.onerror = function () {
      // onerror 后通常紧跟 onclose,统一在 close 里处理 UI
    };

    ws.onclose = function () {
      var wasAuthed = authed;
      authed = false;
      recording = false;
      teardownAudio();
      if (wasAuthed) {
        // 已进入录音屏后断开 → 断线屏
        offlineReason.textContent = '与电脑的连接已中断。';
        showScreen('offline');
      } else if (!isPinScreen()) {
        // 连接过程中失败
        showScreen('pin');
        showPinError('连接失败,请确认电脑端服务正在运行。');
      }
      resetConnectBtn();
    };
  }

  function closeWS() {
    if (ws) {
      ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
      try { ws.close(); } catch (e) {}
      ws = null;
    }
  }

  function handleMessage(msg) {
    if (!msg || typeof msg.type !== 'string') return;

    switch (msg.type) {
      case 'auth':
        if (msg.ok) {
          authed = true;
          busy = false;
          enterRecScreen();
        } else {
          authed = false;
          var reason = msg.reason === 'locked'
            ? '配对已锁定,请在电脑上重新生成配对码。'
            : '配对码错误,请重试。';
          closeWS();
          showScreen('pin');
          showPinError(reason);
          resetConnectBtn();
        }
        break;

      case 'status':
        applyStatusKind(msg);
        break;

      case 'level':
        setLevel(msg.value);
        break;

      case 'busy':
        busy = true;
        recording = false;
        teardownAudioCapture(); // 停止采集但保留 ctx
        updateRecordBtnUI();
        setStatus('电脑忙:' + (msg.reason || '请稍候'), 'error');
        // 短暂后解除忙态,允许重试
        setTimeout(function () {
          busy = false;
          updateRecordBtnUI();
          if (!recording) setStatus('准备就绪', null);
        }, 1500);
        break;
    }
  }

  function applyStatusKind(msg) {
    switch (msg.kind) {
      case 'recording':
        setStatus('🎤 录音中', 'work');
        break;
      case 'transcribing':
        setStatus('🔄 识别中', 'work');
        break;
      case 'polishing':
        setStatus('✨ 润色中', 'work');
        break;
      case 'done':
        var n = (typeof msg.insertedChars === 'number') ? msg.insertedChars : 0;
        setStatus('✅ 已输入 ' + n + ' 字', 'ok');
        setLevel(0);
        break;
      case 'error':
        setStatus('❌ ' + (msg.message || '发生错误'), 'error');
        setLevel(0);
        break;
      default:
        if (msg.message) setStatus(msg.message, null);
    }
  }

  // ============================================================
  // 屏幕状态判断辅助
  // ============================================================
  function isPinScreen() { return screenPin.classList.contains('active'); }

  function enterRecScreen() {
    showPinError('');
    showScreen('rec');
    connDot.style.background = 'var(--ok)';
    syncModeUI();
    updateRecordBtnUI();
    setStatus('准备就绪', null);
    setLevel(0);
  }

  // ============================================================
  // PIN 屏交互
  // ============================================================
  pinInput.addEventListener('input', function () {
    // 仅保留数字
    var v = pinInput.value.replace(/\D+/g, '').slice(0, 6);
    if (v !== pinInput.value) pinInput.value = v;
    showPinError('');
  });
  pinInput.addEventListener('keydown', function (e) {
    if (e.key === 'Enter') doConnect();
  });
  btnConnect.addEventListener('click', doConnect);

  function doConnect() {
    var pin = (pinInput.value || '').replace(/\D+/g, '');
    if (pin.length !== 6) {
      showPinError('请输入 6 位数字配对码。');
      return;
    }
    showPinError('');
    btnConnect.disabled = true;
    btnConnect.textContent = '连接中…';
    connect(pin);
  }

  function showPinError(text) {
    if (!text) {
      pinError.hidden = true;
      pinError.textContent = '';
    } else {
      pinError.hidden = false;
      pinError.textContent = text;
    }
  }
  function resetConnectBtn() {
    btnConnect.disabled = false;
    btnConnect.textContent = '连接';
  }

  // 重新连接
  btnReconnect.addEventListener('click', function () {
    showScreen('pin');
    showPinError('');
    resetConnectBtn();
    if (lastPin) {
      pinInput.value = lastPin;
    }
  });

  // ============================================================
  // 录音按钮交互(toggle / hold)
  // ============================================================
  function updateRecordBtnUI() {
    recordBtn.classList.toggle('recording', recording);
    recordBtn.classList.toggle('busy', busy && !recording);
    if (recording) {
      recordLabel.textContent = (mode === 'hold') ? '松开结束' : '点击结束';
    } else {
      recordLabel.textContent = (mode === 'hold') ? '按住说话' : '点击开始';
    }
  }

  // toggle 模式:click 切换
  recordBtn.addEventListener('click', function () {
    if (mode !== 'toggle') return;
    if (!authed || busy) return;
    if (recording) stopRecording();
    else startRecording();
  });

  // hold 模式:按下/抬起/取消
  recordBtn.addEventListener('pointerdown', function (e) {
    if (mode !== 'hold') return;
    if (!authed || busy) return;
    e.preventDefault();
    try { recordBtn.setPointerCapture(e.pointerId); } catch (err) {}
    if (!recording) startRecording();
  });
  recordBtn.addEventListener('pointerup', function (e) {
    if (mode !== 'hold') return;
    e.preventDefault();
    if (recording) stopRecording();
  });
  recordBtn.addEventListener('pointercancel', function () {
    if (mode !== 'hold') return;
    if (recording) cancelRecording(); // 来电/切后台 → 丢弃
  });
  // hold 时指针滑出按钮也按抬起处理(防止卡在录音态)
  recordBtn.addEventListener('lostpointercapture', function () {
    if (mode !== 'hold') return;
    if (recording) stopRecording();
  });

  // ============================================================
  // 录音流程
  // ============================================================
  function startRecording() {
    if (recording) return;
    if (!ws || ws.readyState !== 1) {
      setStatus('连接已断开', 'error');
      return;
    }
    // 先乐观置态,保证 iOS 在手势同步栈内 resume()
    recording = true;
    updateRecordBtnUI();
    setStatus('正在准备麦克风…', 'work');

    ensureAudio()
      .then(function () {
        if (!recording) {
          // 期间已被取消/松手
          teardownAudioCapture();
          return;
        }
        wsSendJSON({ type: 'start' });
        setStatus('🎤 录音中', 'work');
      })
      .catch(function (err) {
        recording = false;
        updateRecordBtnUI();
        setStatus(micErrorText(err), 'error');
      });
  }

  function stopRecording() {
    if (!recording) return;
    recording = false;
    updateRecordBtnUI();
    teardownAudioCapture();
    wsSendJSON({ type: 'stop' });
    setStatus('🔄 识别中', 'work');
    setLevel(0);
  }

  function cancelRecording() {
    if (!recording) {
      // 即便未在录音也确保采集停掉
      teardownAudioCapture();
      return;
    }
    recording = false;
    updateRecordBtnUI();
    teardownAudioCapture();
    wsSendJSON({ type: 'cancel' });
    setStatus('已取消', null);
    setLevel(0);
  }

  function micErrorText(err) {
    var name = err && err.name ? err.name : '';
    if (name === 'NotAllowedError' || name === 'SecurityError') {
      return '❌ 麦克风权限被拒绝,请在浏览器设置中允许。';
    }
    if (name === 'NotFoundError' || name === 'OverconstrainedError') {
      return '❌ 未找到可用麦克风。';
    }
    if (name === 'NotReadableError') {
      return '❌ 麦克风被其他应用占用。';
    }
    return '❌ 无法启动录音' + (name ? '(' + name + ')' : '') + '。';
  }

  // ============================================================
  // 音频:获取设备 + 建立采集链
  // ============================================================
  // 确保 AudioContext / getUserMedia / 采集节点就绪并开始推流。
  // 必须在用户手势调用栈内(startRecording 由手势触发)。
  function ensureAudio() {
    // 不支持 getUserMedia
    if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
      return Promise.reject(new Error('UNSUPPORTED:浏览器不支持录音,请升级或换浏览器'));
    }

    // 1) AudioContext(iOS 需手势内 resume)
    if (!audioCtx) {
      var AC = window.AudioContext || window.webkitAudioContext;
      if (!AC) {
        return Promise.reject(new Error('UNSUPPORTED:浏览器不支持录音,请升级或换浏览器'));
      }
      audioCtx = new AC();
    }

    var resumeP = (audioCtx.state === 'suspended')
      ? audioCtx.resume().catch(function () {})
      : Promise.resolve();

    return resumeP
      .then(function () {
        // 2) 麦克风流(已存在则复用)
        if (mediaStream) return mediaStream;
        return navigator.mediaDevices.getUserMedia({
          audio: {
            channelCount: 1,
            echoCancellation: true,
            noiseSuppression: true,
            autoGainControl: true
          },
          video: false
        }).then(function (stream) {
          mediaStream = stream;
          return stream;
        });
      })
      .then(function (stream) {
        // 3) 建立采集图(若已建好则跳过)
        if (sourceNode) return;
        sourceNode = audioCtx.createMediaStreamSource(stream);
        return buildCaptureGraph();
      });
  }

  // 建立 AudioWorklet(优先)或 ScriptProcessor(兜底)
  function buildCaptureGraph() {
    var inSr = audioCtx.sampleRate || 48000;

    // 优先 AudioWorklet
    if (audioCtx.audioWorklet && typeof AudioWorkletNode !== 'undefined') {
      return loadWorklet()
        .then(function () {
          workletNode = new AudioWorkletNode(audioCtx, 'ol-pcm-worklet', {
            numberOfInputs: 1,
            numberOfOutputs: 0,
            channelCount: 1,
            processorOptions: { inSr: inSr, targetSr: TARGET_SR }
          });
          workletNode.port.onmessage = function (e) {
            // e.data 是已转换好的 Int16 LE ArrayBuffer
            sendAudio(e.data);
          };
          sourceNode.connect(workletNode);
          usingWorklet = true;
        })
        .catch(function () {
          // worklet 加载失败 → 回退 ScriptProcessor
          usingWorklet = false;
          buildScriptProcessor(inSr);
        });
    }

    // 无 audioWorklet:直接兜底
    usingWorklet = false;
    buildScriptProcessor(inSr);
    return Promise.resolve();
  }

  // ---- AudioWorklet processor(字符串 → Blob URL 加载) ----
  function loadWorklet() {
    if (workletUrl) return audioCtx.audioWorklet.addModule(workletUrl);

    var code =
      'class OlPcmWorklet extends AudioWorkletProcessor {' +
      '  constructor(o){' +
      '    super();' +
      '    var p=(o&&o.processorOptions)||{};' +
      '    this.inSr=p.inSr||sampleRate;' +
      '    this.targetSr=p.targetSr||16000;' +
      '    this.ratio=this.inSr/this.targetSr;' +
      '    this.phase=0;' +       // 当前小数相位
      '    this.last=0;' +        // 上一块最后一个样本(用于跨块拼接)
      '    this.hasLast=false;' +
      '  }' +
      '  process(inputs){' +
      '    var ch=inputs[0]&&inputs[0][0];' +
      '    if(!ch||ch.length===0){return true;}' +
      '    var ratio=this.ratio;' +
      '    var phase=this.phase;' +
      '    var prev=this.last;' +
      '    var hasPrev=this.hasLast;' +
      '    var n=ch.length;' +
      // 估算输出样本数上界
      '    var outCap=Math.ceil((n+1)/ratio)+2;' +
      '    var pcm=new ArrayBuffer(outCap*2);' +
      '    var dv=new DataView(pcm);' +
      '    var oi=0;' +
      // 线性插值:phase 以"输入样本"为单位推进,step=inSr/16000
      // i=floor(phase),frac=phase-i;a=样本[i],b=样本[i+1]
      // 跨块时 i 可能为 -1,用 prev 作为 a。
      '    while(true){' +
      '      var i=Math.floor(phase);' +
      '      var frac=phase-i;' +
      '      var a,b;' +
      '      if(i+1>=n){break;}' +            // 需要 i 和 i+1 都在块内(或 a 用 prev)
      '      if(i<0){' +
      '        if(!hasPrev){phase+=ratio;continue;}' +
      '        a=prev;b=ch[0];' +
      '      }else{' +
      '        a=ch[i];b=ch[i+1];' +
      '      }' +
      '      var s=a+(b-a)*frac;' +
      '      if(s>1)s=1;else if(s<-1)s=-1;' +
      '      dv.setInt16(oi*2, (s*32767)|0, true);' +
      '      oi++;' +
      '      phase+=ratio;' +
      '    }' +
      // 保留余数:把 phase 拉回到相对下一块起点
      '    this.phase=phase-n;' +
      '    this.last=ch[n-1];' +
      '    this.hasLast=true;' +
      '    if(oi>0){' +
      '      var out=pcm.slice(0,oi*2);' +
      '      this.port.postMessage(out,[out]);' +
      '    }' +
      '    return true;' +
      '  }' +
      '}' +
      'registerProcessor("ol-pcm-worklet", OlPcmWorklet);';

    workletUrl = URL.createObjectURL(new Blob([code], { type: 'application/javascript' }));
    return audioCtx.audioWorklet.addModule(workletUrl);
  }

  // ---- ScriptProcessor 兜底 ----
  function buildScriptProcessor(inSr) {
    scriptNode = audioCtx.createScriptProcessor(4096, 1, 1);
    resampleState.phase = 0;
    resampleState.last = 0;
    resampleState.hasLast = false;

    scriptNode.onaudioprocess = function (e) {
      if (!recording) return;
      var input = e.inputBuffer.getChannelData(0);
      var buf = resampleToInt16LE(input, inSr);
      if (buf && buf.byteLength) sendAudio(buf);
    };
    // ScriptProcessor 需连到 destination 才会触发(用静音增益避免回放)
    sourceNode.connect(scriptNode);
    var silent = audioCtx.createGain();
    silent.gain.value = 0;
    scriptNode.connect(silent);
    silent.connect(audioCtx.destination);
    scriptNode._silentGain = silent;
  }

  // 主线程线性插值重采样(给 ScriptProcessor 用),逻辑与 worklet 一致
  function resampleToInt16LE(ch, inSr) {
    var ratio = inSr / TARGET_SR;
    var phase = resampleState.phase;
    var prev = resampleState.last;
    var hasPrev = resampleState.hasLast;
    var n = ch.length;
    if (n === 0) return null;

    var outCap = Math.ceil((n + 1) / ratio) + 2;
    var pcm = new ArrayBuffer(outCap * 2);
    var dv = new DataView(pcm);
    var oi = 0;

    while (true) {
      var i = Math.floor(phase);
      var frac = phase - i;
      var a, b;
      if (i + 1 >= n) break;
      if (i < 0) {
        if (!hasPrev) { phase += ratio; continue; }
        a = prev; b = ch[0];
      } else {
        a = ch[i]; b = ch[i + 1];
      }
      var s = a + (b - a) * frac;
      if (s > 1) s = 1; else if (s < -1) s = -1;
      dv.setInt16(oi * 2, (s * 32767) | 0, true);
      oi++;
      phase += ratio;
    }

    resampleState.phase = phase - n;
    resampleState.last = ch[n - 1];
    resampleState.hasLast = true;

    return oi > 0 ? pcm.slice(0, oi * 2) : null;
  }

  // 发送二进制音频帧(仅录音中且连接可用)
  function sendAudio(buf) {
    if (!recording) return;
    if (ws && ws.readyState === 1 && buf && buf.byteLength) {
      try { ws.send(buf); } catch (e) {}
    }
  }

  // ============================================================
  // 音频清理
  // ============================================================
  // 仅停止"采集/推流"(断开节点),保留 audioCtx & mediaStream 以便快速重启。
  function teardownAudioCapture() {
    try { if (workletNode) { workletNode.port.onmessage = null; workletNode.disconnect(); } } catch (e) {}
    workletNode = null;

    try {
      if (scriptNode) {
        scriptNode.onaudioprocess = null;
        scriptNode.disconnect();
        if (scriptNode._silentGain) {
          try { scriptNode._silentGain.disconnect(); } catch (e2) {}
        }
      }
    } catch (e) {}
    scriptNode = null;

    try { if (sourceNode) sourceNode.disconnect(); } catch (e) {}
    // sourceNode 置空,下次 ensureAudio 重新从 stream 创建
    sourceNode = null;

    // 复位兜底重采样状态
    resampleState.phase = 0;
    resampleState.last = 0;
    resampleState.hasLast = false;
  }

  // 彻底释放(断线时):停止麦克风轨道并关闭 ctx。
  function teardownAudio() {
    teardownAudioCapture();
    if (mediaStream) {
      try {
        var tracks = mediaStream.getTracks();
        for (var i = 0; i < tracks.length; i++) tracks[i].stop();
      } catch (e) {}
      mediaStream = null;
    }
    // 不强行 close ctx(部分浏览器再次 new 较慢);仅在确实需要时挂起
    if (audioCtx && audioCtx.state === 'running') {
      try { audioCtx.suspend(); } catch (e) {}
    }
  }

  // ============================================================
  // 页面可见性:切后台时若在 hold 录音则取消,避免半截音频
  // ============================================================
  document.addEventListener('visibilitychange', function () {
    if (document.hidden && recording) {
      cancelRecording();
    }
  });

  // ============================================================
  // 初始化
  // ============================================================
  function init() {
    syncModeUI();
    showScreen('pin');
    showPinError('');
    // 自动聚焦 PIN(部分移动端会被策略拦截,忽略失败)
    setTimeout(function () { try { pinInput.focus(); } catch (e) {} }, 200);
  }

  init();
})();
