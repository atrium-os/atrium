// Run with Playwright available to Node. CHROME_BIN may name a local Chromium.
const assert = require('node:assert/strict');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const { chromium } = require('playwright');

// Expected first 45 minutes of each eligible free interval, calculated by hand.
const cases = [
  ['Review day', 'Team check-in', ['09:30–10:15', '13:00–13:45']],
  ['Packed morning', 'Studio session', []],
  ['One exact fit', 'Workshop', ['10:00–10:45']],
  ['Overlapping meetings', 'Early review', ['11:00–11:45']],
  ['Open morning', '300 min open', ['09:00–09:45']],
  ['Mixed invitations', 'Optional office hours · free', ['10:00–10:45']],
  ['Four openings', 'Stand-up', ['09:00–09:45', '10:00–10:45', '11:00–11:45', '12:00–12:45']]
];

async function check() {
  const browser = await chromium.launch({ executablePath: process.env.CHROME_BIN });
  try {
    const page = await browser.newPage({ viewport: { width: 940, height: 720 } });
    page.setDefaultTimeout(4000);
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    const file = path.resolve(__dirname, '../docs/design/calendar-planning.html');
    await page.goto(pathToFileURL(file).href);
    const agenda = page.locator('#cal-agenda');
    const holdButtons = agenda.getByRole('button', { name: /^Hold 45 min/ });
    const boxes = page.locator('#cal-options input');
    await page.getByRole('button', { name: 'Find 45 min' }).click();
    for (const [sample, marker, expected] of cases) {
      await page.getByLabel('Sample schedule').selectOption(sample);
      assert(await agenda.getByText(marker, {exact:true}).isVisible(), sample);
      const original = await agenda.innerText();
      assert.equal(await holdButtons.count(), expected.length, sample);
      await page.getByRole('button', {name:'Offer times…'}).click();
      assert.deepEqual(await page.locator('#cal-options label').allTextContents(), expected, sample);
      const message = page.locator('#cal-message');
      if (expected.length) {
        const text = 'Would ' + expected.slice(0,3).join(' or ') + ' on Tuesday 24 November 2026 work for you? Kolkata (UTC+05:30).';
        assert.equal(await message.innerText(), text, sample);
        if (expected.length > 3) {
          assert(await boxes.nth(3).isDisabled());
          await boxes.first().uncheck();
          await boxes.nth(3).check();
          const revised = (await message.innerText());
          assert(!revised.includes(expected[0]) && revised.includes(expected[3]));
          assert.equal(await page.locator('#cal-options input:checked').count(), 3);
        }
      } else {
        assert.equal(await message.innerText(), 'No 45-minute opening before Budget review.');
      }
      await page.getByRole('button',{name:'Close preview'}).click();
      assert.equal(await agenda.innerText(), original, sample + ' preview wrote a hold');
      for (let index=0; index<expected.length; index++) {
        await holdButtons.nth(index).click();
        assert.deepEqual(await agenda.locator('.held .time').allTextContents(), [expected[index]]);
        await page.getByRole('button',{name:'Week',exact:true}).click();
        assert.equal(await agenda.getByText(expected[index]+' · Preparation · Private hold',{exact:true}).count(),1);
        await page.getByRole('button',{name:'Offer times…'}).click();
        const remaining = await page.locator('#cal-options label').allTextContents();
        assert(!remaining.includes(expected[index]), sample);
        for (const other of expected.filter(time=>time!==expected[index])) assert(remaining.includes(other), sample);
        if (sample==='Open morning') assert.deepEqual(remaining,['09:45–10:30']);
        await page.getByRole('button',{name:'Close preview'}).click();
        await agenda.getByRole('button',{name:/Release hold/}).click();
        assert.equal(await agenda.innerText(), original, sample + ' release did not restore availability');
      }
      console.log('PASS',sample,'·',expected.length,'eligible gaps');
    }
    await page.setViewportSize({width:320,height:760});
    await page.getByRole('button',{name:'Offer times…'}).click();
    assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.getByText('Try notification scenarios').click();
    const activity = page.getByLabel('Sample activity');
    const alertCases = [
      ['moves','1 individual alert · 0 sounds · Latest change: 14:00 → 14:30'],
      ['list','0 individual alerts · 0 sounds'],
      ['wake','0 individual alerts · 0 sounds · 1 silent summary · 1 upcoming · 18 in history'],
      ['fresh','1 individual alert · 1 sound']
    ];
    for (const [sample, expected] of alertCases) {
      await activity.selectOption(sample);
      await page.getByRole('button',{name:'Replay sample'}).click();
      const result = await page.locator('#cal-alert-result').innerText();
      assert(result.endsWith(expected), result);
      console.log('PASS',result);
    }
    assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    assert.deepEqual(errors,[]);
  } finally {
    await browser.close();
  }
}
check().catch(error=>{ console.error(error); process.exitCode=1; });
