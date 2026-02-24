# Assets

- **icon.ico** – Windows executable icon (File Explorer, taskbar). Source: [VimAndTonic](https://www.youtube.com/@VimAndTonic) YouTube channel profile image.
- **avatar.png** – Original profile image used to generate `icon.ico`.

To regenerate the icon after updating the avatar:

```bash
python3 -c "
from PIL import Image
img = Image.open('assets/avatar.png')
img.save('assets/icon.ico', format='ICO', sizes=[(256,256), (48,48), (32,32), (16,16)])
"
```
