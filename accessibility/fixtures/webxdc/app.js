webxdc.setUpdateListener(update => {
  document.getElementById('state').textContent = update.payload.message;
}, 0);
document.getElementById('send').addEventListener('click', async () => {
  try {
    await webxdc.sendUpdate({payload:{message:'Shared update received'}}, 'Fixture update');
  } catch {
    document.getElementById('state').textContent = 'Update failed';
  }
});
