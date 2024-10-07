Environment variables to control the app

- `WIDTH` the width of the offscreen canvas
- `HEIGHT` the height of the offscreen canvas
- `PORT` controls the port of the http server (3002 is default)

On a VPS without GPU lavapipe should be used:
```
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
```

If `DISCORD_TOKEN` & `GUILD_ID` are present discord support will be activated.

