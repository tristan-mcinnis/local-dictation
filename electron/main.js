const { app, BrowserWindow, Tray, Menu, ipcMain, shell, globalShortcut, screen } = require('electron');
const path = require('path');
const fs = require('fs');
const { spawn } = require('child_process');
const Store = require('electron-store');

const store = new Store();
let tray = null;
let settingsWindow = null;
let historyWindow = null;
let visualizerWindow = null;
let debugWindow = null;
let pythonProcess = null;
let logBuffer = [];

const isDev = process.argv.includes('--dev') || process.env.NODE_ENV === 'development';
const pythonPath = isDev 
  ? path.join(__dirname, '..', 'src')
  : path.join(process.resourcesPath, 'python');

const transcriptsDir = path.join(app.getPath('userData'), 'transcripts');
if (!fs.existsSync(transcriptsDir)) {
  fs.mkdirSync(transcriptsDir, { recursive: true });
}

function createTray() {
  const iconPath = path.join(__dirname, 'assets', 'tray-icon.png');
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

function createVisualizerWindow() {
  const display = screen.getPrimaryDisplay();
  const { width, height } = display.workAreaSize;
  
  visualizerWindow = new BrowserWindow({
    width: 120,
    height: 40,
    x: Math.floor((width - 120) / 2),
    y: height - 60,
    frame: false,
    transparent: true,
    alwaysOnTop: true,
    hasShadow: false,
    resizable: false,
    movable: false,
    focusable: false,
    skipTaskbar: true,
    webPreferences: {
      nodeIntegration: true,
      contextIsolation: false
    }
  });
  
  visualizerWindow.loadFile('visualizer_realtime.html');
  visualizerWindow.setVisibleOnAllWorkspaces(true);
  visualizerWindow.setAlwaysOnTop(true, 'floating');
  visualizerWindow.setIgnoreMouseEvents(true);
  visualizerWindow.setFocusable(false);
  visualizerWindow.hide();
  
  visualizerWindow.on('closed', () => {
    visualizerWindow = null;
  });
}

function showSettings() {
  if (settingsWindow) {
    settingsWindow.show();
    return;
  }
  
  settingsWindow = new BrowserWindow({
    width: 500,
    height: 400,
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

function addLog(message, level = 'info') {
  const log = {
    message,
    level,
    timestamp: new Date().toISOString()
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
  console.log(`[${level.toUpperCase()}] ${message}`);
}

function startPythonBackend() {
  const model = store.get('model', 'medium.en');
  const chord = store.get('hotkey', 'CMD,ALT');
  
  addLog(`Starting Python backend with model: ${model}, chord: ${chord}`, 'info');
  
  if (pythonProcess) {
    addLog('Killing existing Python process', 'warning');
    pythonProcess.kill();
    pythonProcess = null;
  }
  
  try {
    const pythonCmd = isDev 
      ? ['uv', 'run', 'python', '-m', 'local_dictation.cli_electron', '--model', model, '--chord', chord]
      : [path.join(pythonPath, 'local-dictation'), '--model', model, '--chord', chord];
    
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
        addLog('Recording started', 'info');
      } else if (message.startsWith('RECORDING_STOP')) {
        addLog('Recording stopped', 'info');
      } else if (message.startsWith('TRANSCRIPT:')) {
        const transcript = message.substring(11);
        addLog(`Transcript: ${transcript}`, 'success');
        saveTranscript(transcript);
      } else if (message.startsWith('READY:')) {
        const info = message.substring(6);
        addLog(`Python backend ready: ${info}`, 'success');
      } else if (message.startsWith('ERROR:')) {
        const error = message.substring(6);
        addLog(`Python error: ${error} [E004]`, 'error');
      } else if (message.startsWith('TYPED:')) {
        addLog('Text typed successfully', 'success');
      } else if (message.startsWith('TYPE_ERROR:')) {
        const error = message.substring(11);
        addLog(`Failed to type text: ${error} [E005]`, 'error');
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

ipcMain.handle('get-settings', () => {
  return {
    model: store.get('model', 'medium.en'),
    hotkey: store.get('hotkey', 'CMD,ALT')
  };
});

ipcMain.handle('save-settings', (event, settings) => {
  store.set('model', settings.model);
  store.set('hotkey', settings.hotkey);
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
  // createVisualizerWindow(); // Disabled for testing
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