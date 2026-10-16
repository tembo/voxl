import { createRequire } from 'module';
const require = createRequire(
	process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/'
);
const { chromium } = require('playwright');
const text = process.argv[2] || 'aA!ä';
const b = await chromium.launch({
	executablePath: process.env.CHROMIUM || 'chromium',
	args: ['--no-sandbox'],
});
const p = await b.newPage({ viewport: { width: 1280, height: 800 } });
await p.goto('http://localhost:6902/');
await p.waitForTimeout(2000);
await p.mouse.click(1025, 475);
await p.waitForTimeout(300);
await p.keyboard.type(text, { delay: 30 });
await p.waitForTimeout(500);
await b.close();
