const { app, BrowserWindow, Tray, Menu, ipcMain, shell, globalShortcut } = require('electron');
const path = require('path');
const fs = require('fs');
const { spawn } = require('child_process');
const Store = require('electron-store');

const store = new Store();
let tray = null;
let settingsWindow = null;
let historyWindow = null;
let debugWindow = null;
let pythonProcess = null;
let logBuffer = [];
let performanceTimers = {};

const isDev = process.argv.includes('--dev') || process.env.NODE_ENV === 'development';
const pythonPath = isDev 
  ? path.join(__dirname, '..', 'src')
  : path.join(process.resourcesPath, 'python');

const transcriptsDir = path.join(app.getPath('userData'), 'transcripts');
if (!fs.existsSync(transcriptsDir)) {
  fs.mkdirSync(transcriptsDir, { recursive: true });
}

function createTray() {
  const iconPath = path.join(__dirname, 'assets', 'trayTemplate.png');
  tray = new Tray(iconPath);
  
  const contextMenu = Menu.buildFromTemplate([
    {
      label: 'Settings',
      click: () => showSettings()
    },
    {
      label: 'Transcript History',
      click: () => showHistory()
    },
    {
      label: 'Debug Logs',
      click: () => showDebugLogs()
    },
    { type: 'separator' },
    {
      label: 'Start Recording',
      accelerator: store.get('hotkey', 'Cmd+Alt'),
      click: () => startRecording()
    },
    { type: 'separator' },
    {
      label: 'Quit',
      click: () => {
        if (pythonProcess) pythonProcess.kill();
        app.quit();
      }
    }
  ]);
  
  tray.setToolTip('Local Dictation');
  tray.setContextMenu(contextMenu);
}

function showSettings() {
  if (settingsWindow) {
    settingsWindow.show();
    return;
  }
  
  settingsWindow = new BrowserWindow({
    width: 500,
    height: 520,
    resizable: false,
    titleBarStyle: 'hiddenInset',
    webPreferences: {
      nodeIntegration: true,
      contextIsolation: false
    }
  });
  
  settingsWindow.loadFile('settings.html');
  
  settingsWindow.on('closed', () => {
    settingsWindow = null;
  });
}

function showHistory() {
  if (historyWindow) {
    historyWindow.show();
    return;
  }
  
  historyWindow = new BrowserWindow({
    width: 700,
    height: 500,
    titleBarStyle: 'hiddenInset',
    webPreferences: {
      nodeIntegration: true,
      contextIsolation: false
    }
  });
  
  historyWindow.loadFile('history.html');
  
  historyWindow.on('closed', () => {
    historyWindow = null;
  });
}

function showDebugLogs() {
  if (debugWindow) {
    debugWindow.show();
    return;
  }
  
  debugWindow = new BrowserWindow({
    width: 900,
    height: 600,
    titleBarStyle: 'hiddenInset',
    webPreferences: {
      nodeIntegration: true,
      contextIsolation: false,
      enableRemoteModule: true
    }
  });
  
  debugWindow.loadFile('debug.html');
  
  debugWindow.on('closed', () => {
    debugWindow = null;
  });
  
  // Send buffered logs to debug window
  debugWindow.webContents.on('did-finish-load', () => {
    logBuffer.forEach(log => {
      debugWindow.webContents.send('log-message', log);
    });
  });
}

function addLog(message, level = 'info', metrics = null) {
  const now = Date.now();
  const log = {
    message,
    level,
    timestamp: new Date().toISOString(),
    milliseconds: now,
    metrics: metrics
  };
  
  logBuffer.push(log);
  
  // Keep only last 1000 logs in buffer
  if (logBuffer.length > 1000) {
    logBuffer.shift();
  }
  
  // Send to debug window if open
  if (debugWindow && !debugWindow.isDestroyed()) {
    debugWindow.webContents.send('log-message', log);
  }
  
  // Also log to console for development
  const metricsStr = metrics ? ` [${metrics}ms]` : '';
  console.log(`[${level.toUpperCase()}] ${message}${metricsStr}`);
}

function startTimer(name) {
  performanceTimers[name] = Date.now();
}

function endTimer(name) {
  if (performanceTimers[name]) {
    const elapsed = Date.now() - performanceTimers[name];
    delete performanceTimers[name];
    return elapsed;
  }
  return null;
}

function startPythonBackend() {
  const model = store.get('model', 'medium.en');
  // If using an English-only model, force language to 'en'
  const defaultLang = model.endsWith('.en') ? 'en' : 'auto';
  const language = store.get('language', defaultLang);
  const chord = store.get('hotkey', 'CMD,ALT');
  const assistantMode = store.get('assistantMode', false);
  const assistantModel = store.get('assistantModel', 'mlx-community/Qwen3-1.7B-4bit');
  const emailFormatting = store.get('emailFormatting', true);
  const emailSignOff = store.get('emailSignOff', 'Best regards,\n[Your Name]');
  
  addLog(`Starting Python backend with model: ${model}, language: ${language}, chord: ${chord}, assistant: ${assistantMode}`, 'info');
  
  if (pythonProcess) {
    addLog('Killing existing Python process', 'warning');
    pythonProcess.kill();
    pythonProcess = null;
  }
  
  try {
    const pythonCmd = isDev 
      ? ['uv', 'run', 'python', '-m', 'local_dictation.cli_electron', '--model', model, '--lang', language, '--chord', chord]
      : [path.join(pythonPath, 'local-dictation'), '--model', model, '--lang', language, '--chord', chord];
    
    if (assistantMode) {
      pythonCmd.push('--assistant-mode');
      pythonCmd.push('--assistant-model', assistantModel);
      
      // Pass email settings via environment variables to avoid command line escaping issues
      process.env.EMAIL_FORMATTING = emailFormatting ? 'true' : 'false';
      process.env.EMAIL_SIGN_OFF = emailSignOff;
    }
    
    const options = {
      stdio: ['pipe', 'pipe', 'pipe'],
      cwd: isDev ? path.join(__dirname, '..') : undefined,
      env: { ...process.env }
    };
    
    addLog(`Command: ${pythonCmd[0]} ${pythonCmd.slice(1).join(' ')}`, 'debug');
    addLog(`Working directory: ${options.cwd}`, 'debug');
    
    pythonProcess = spawn(pythonCmd[0], pythonCmd.slice(1), options);
  } catch (error) {
    addLog(`Failed to start Python backend: ${error.message} [E001]`, 'error');
    return;
  }
  
  pythonProcess.stdout.on('data', (data) => {
    const messages = data.toString().split('\n').filter(m => m.trim());
    
    for (const message of messages) {
      addLog(`Python: ${message}`, 'debug');
      
      if (message.startsWith('RECORDING_START')) {
        startTimer('recording');
        startTimer('full_cycle');
        addLog('Recording started', 'info');
      } else if (message.startsWith('RECORDING_STOP')) {
        const recordingTime = endTimer('recording');
        startTimer('transcription');
        addLog(`Recording stopped`, 'info', recordingTime);
      } else if (message.startsWith('TRANSCRIPT:')) {
        const transcript = message.substring(11);
        const transcriptionTime = endTimer('transcription');
        startTimer('typing');
        addLog(`Transcript: ${transcript}`, 'success', transcriptionTime);
        saveTranscript(transcript);
      } else if (message.startsWith('READY:')) {
        const info = message.substring(6);
        addLog(`Python backend ready: ${info}`, 'success');
      } else if (message.startsWith('ERROR:')) {
        const error = message.substring(6);
        addLog(`Python error: ${error} [E004]`, 'error');
      } else if (message.startsWith('TYPED:')) {
        const typingTime = endTimer('typing');
        const fullCycleTime = endTimer('full_cycle');
        addLog(`Text typed successfully`, 'success', typingTime);
        if (fullCycleTime) {
          addLog(`Full cycle completed`, 'info', fullCycleTime);
        }
      } else if (message.startsWith('TYPE_ERROR:')) {
        const error = message.substring(11);
        endTimer('typing');
        endTimer('full_cycle');
        addLog(`Failed to type text: ${error} [E005]`, 'error');
      } else if (message.startsWith('COMMAND_PROCESSED:')) {
        const command = message.substring(18);
        const fullCycleTime = endTimer('full_cycle');
        addLog(`Assistant command processed: ${command}`, 'success', fullCycleTime);
      } else if (message.startsWith('ASSISTANT_MODE:')) {
        const status = message.substring(15);
        addLog(`Assistant mode ${status}`, 'info');
      }
    }
  });
  
  pythonProcess.stderr.on('data', (data) => {
    const message = data.toString().trim();
    if (message) {
      addLog(`Python stderr: ${message}`, 'debug');
    }
  });
  
  pythonProcess.on('error', (error) => {
    addLog(`Python process error: ${error.message} [E002]`, 'error');
    pythonProcess = null;
  });
  
  pythonProcess.on('close', (code) => {
    if (code !== 0) {
      addLog(`Python process exited with code ${code} [E002]`, 'error');
    } else {
      addLog(`Python process exited normally`, 'info');
    }
    pythonProcess = null;
  });
}

function saveTranscript(text) {
  const now = new Date();
  const timestamp = now.toISOString().replace(/[:.]/g, '-').slice(0, -5);
  const filename = `${timestamp}.txt`;
  const filepath = path.join(transcriptsDir, filename);
  
  fs.writeFileSync(filepath, text, 'utf8');
  
  if (historyWindow) {
    historyWindow.webContents.send('new-transcript', { filename, text, timestamp: now });
  }
}

function startRecording() {
  if (pythonProcess && pythonProcess.stdin) {
    pythonProcess.stdin.write('START\n');
  }
}

function toggleAssistantMode(enabled) {
  if (pythonProcess && pythonProcess.stdin) {
    pythonProcess.stdin.write(`TOGGLE_ASSISTANT:${enabled}\n`);
  }
}

ipcMain.handle('get-settings', () => {
  const model = store.get('model', 'medium.en');
  // If using an English-only model by default, default to 'en' language
  const defaultLang = model.endsWith('.en') ? 'en' : 'auto';
  return {
    model: model,
    language: store.get('language', defaultLang),
    hotkey: store.get('hotkey', 'CMD,ALT'),
    assistantMode: store.get('assistantMode', false),
    assistantModel: store.get('assistantModel', 'mlx-community/Llama-3.2-3B-Instruct-4bit'),
    emailFormatting: store.get('emailFormatting', true),
    emailSignOff: store.get('emailSignOff', 'Best regards,\n[Your Name]')
  };
});

ipcMain.handle('save-settings', (event, settings) => {
  store.set('model', settings.model);
  store.set('language', settings.language);
  store.set('hotkey', settings.hotkey);
  store.set('assistantMode', settings.assistantMode);
  store.set('assistantModel', settings.assistantModel);
  store.set('emailFormatting', settings.emailFormatting);
  store.set('emailSignOff', settings.emailSignOff);
  startPythonBackend();
  return true;
});

ipcMain.handle('get-transcripts', () => {
  const files = fs.readdirSync(transcriptsDir)
    .filter(f => f.endsWith('.txt'))
    .sort((a, b) => b.localeCompare(a))
    .slice(0, 100);
  
  return files.map(filename => {
    const filepath = path.join(transcriptsDir, filename);
    const text = fs.readFileSync(filepath, 'utf8');
    const stats = fs.statSync(filepath);
    return {
      filename,
      text,
      timestamp: stats.mtime
    };
  });
});

ipcMain.handle('delete-transcript', (event, filename) => {
  const filepath = path.join(transcriptsDir, filename);
  fs.unlinkSync(filepath);
  return true;
});

ipcMain.handle('open-transcript-folder', () => {
  shell.openPath(transcriptsDir);
});

app.whenReady().then(() => {
  createTray();
  startPythonBackend();
  
  app.dock.hide();
});

app.on('window-all-closed', (e) => {
  e.preventDefault();
});

app.on('will-quit', () => {
  if (pythonProcess) {
    pythonProcess.kill();
  }
});