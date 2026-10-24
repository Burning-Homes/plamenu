import { test, expect } from '../fixtures.mjs';
import AxeBuilder from '@axe-core/playwright';
import { signIn, chooseInterfaceLanguage } from '../session.mjs';

const png = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=', 'base64');

test('Russian media upload and viewer keep keyboard focus and localized controls', async ({page}, info) => {
  await signIn(page);
  try {
    await chooseInterfaceLanguage(page, 'ru');
    await page.goto('/compose');
    await page.locator('[name=status]').fill(`Медиа для проверки ${info.project.name}`);
    const chooser = page.waitForEvent('filechooser');
    await page.locator('.compose__dropzone').press('Enter');
    await (await chooser).setFiles([
      {name:'first.png', mimeType:'image/png', buffer:png},
      {name:'second.png', mimeType:'image/png', buffer:png},
    ]);
    const descriptions = ['Тестовое изображение номер один. ' + 'Подробное описание изображения. '.repeat(24), 'Тестовое изображение номер два.'];
    const cards = page.locator('.compose__attachment');
    await expect(cards).toHaveCount(2);
    for (let index=0; index<2; index++) {
      const card=cards.nth(index);
      const details=card.locator('details').filter({has:page.locator('.compose__alt')});
      if (await details.getAttribute('open') === null) await details.locator(':scope > summary').press('Enter');
      await card.locator('.compose__alt').fill(descriptions[index]);
    }
    await page.locator('button[name=op][value=post]').press('Enter');
    await page.waitForURL(url=>/\/@[^/]+\/\d+/.test(url.pathname));
    const opener=page.locator('.status__media a.media__link').first();
    await opener.press('Enter');
    const dialog=page.getByRole('dialog');
    await expect(dialog).toBeVisible();
    await expect(dialog.locator('.lightbox__close')).toBeFocused();
    await expect.soft(dialog).toHaveAccessibleName('Просмотр медиа', {timeout:1500});
    await expect.soft(dialog.locator('.lightbox__close')).toHaveAccessibleName('Закрыть', {timeout:1500});
    await expect.soft(dialog.locator('.lightbox__nav--next')).toHaveAccessibleName('Следующее', {timeout:1500});
    for (let index=0; index<8; index++) {
      await page.keyboard.press('Tab');
      expect(await dialog.evaluate(el=>el.contains(document.activeElement))).toBe(true);
    }
    await dialog.locator('.lightbox__nav--next').press('Enter');
    await expect(dialog.locator('.lightbox__alt')).toHaveText(descriptions[1]);
    await page.screenshot({path:info.outputPath('media-navigation-focus.png'), fullPage:true});
    await expect.soft(dialog.locator('.lightbox__nav--prev')).toBeFocused({timeout:1500});
    await expect(dialog.locator('.lightbox__slide[aria-hidden=false] img')).toHaveAttribute('alt', descriptions[1]);
    await dialog.locator('.lightbox__nav--prev').press('Enter');
    await expect.soft(dialog.locator('.lightbox__nav--next')).toBeFocused({timeout:1500});
    const more=dialog.locator('.lightbox__alt-toggle');
    await expect(more).toBeVisible();
    await more.press('Enter');
    await expect(more).toHaveAttribute('aria-expanded','true');
    await expect.soft(more).toHaveText('Показать меньше', {timeout:1500});
    await page.screenshot({path:info.outputPath('media-viewer-ru.png'), fullPage:true});
    if (info.project.name.includes('mobile')) {
      const description=dialog.locator('.lightbox__alt');
      await page.keyboard.press('Shift+Tab');
      await page.screenshot({path:info.outputPath('media-description-scroll.png'), fullPage:true});
      await expect.soft(description).toBeFocused({timeout:1500});
      if (await description.evaluate(el=>el===document.activeElement)) {
        await page.keyboard.press('ArrowDown');
        await expect.poll(()=>description.evaluate(el=>el.scrollTop)).toBeGreaterThan(0);
      }
    } else {
      const viewport=page.viewportSize();
      await page.setViewportSize({width:320,height:800});
      await expect(dialog.locator('.lightbox__alt')).toHaveAttribute('tabindex','0');
      await page.setViewportSize(viewport);
      await expect(dialog.locator('.lightbox__alt')).toHaveAttribute('tabindex','-1');
    }
    const result=await new AxeBuilder({page}).withTags(['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']).analyze();
    expect.soft(result.violations.map(v=>({id:v.id,nodes:v.nodes.map(n=>n.target)}))).toEqual([]);
    await page.keyboard.press('ArrowRight');
    await expect(dialog.locator('.lightbox__alt')).toHaveText(descriptions[1]);
    await expect.soft(dialog.locator('.lightbox__close')).toBeFocused({timeout:1500});
    await page.keyboard.press('Escape');
    await expect(dialog).toBeHidden();
    await expect(opener).toBeFocused();
  } finally {
    await chooseInterfaceLanguage(page, 'en');
  }
});

test('@no-js native media upload preserves its description and opens the image', async ({page}) => {
  await signIn(page);
  await page.goto('/compose');
  await page.locator('[name=status]').fill('Native keyboard image upload');
  const slot=page.locator('.compose__media-slot').first();
  const description='One tiny test image uploaded without JavaScript.';
  await slot.locator('[name="media_alt[]"]').fill(description);
  const chooser=page.waitForEvent('filechooser');
  await slot.locator('input[name="media[]"]').press('Enter');
  await (await chooser).setFiles({name:'native.png',mimeType:'image/png',buffer:png});
  await page.locator('button[name=op][value=post]').press('Enter');
  await page.waitForURL(url=>/\/@[^/]+\/\d+/.test(url.pathname));
  const link=page.locator('.status__media a.media__link').first();
  await expect(link.locator('img')).toHaveAttribute('alt',description);
  await expect(link).toHaveAccessibleName(/One tiny test image uploaded without JavaScript.*Open image/);
  const opened=page.waitForEvent('popup');
  await link.press('Enter');
  const image=await opened;
  try { await expect(image).toHaveURL(/\/media\//); }
  finally { await image.close(); }
});
