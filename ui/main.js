function createTerminal() {
    const term = new Terminal({
        fontFamily: '"FiraCode Nerd Font", monospace'
        // fontFamily: 'Courier New'
    });
    term.open(document.getElementById("terminal"));
    console.log('Terminal options', term.options);
    term.write('Hello from \x1B[1;3;31mxterm.js\x1B[0m!\r\n');
    connect(term);
}

function connect(term) {
    const ws = new WebSocket(`ws://${window.location.host}/ws`);
    ws.addEventListener('open', () => {
        console.log('WebSocket connected!');
        term.loadAddon(new AttachAddon.AttachAddon(ws));
    });
    ws.addEventListener('close', () => {
        term.write('<disconnected>\r\n');
    });
}

createTerminal();