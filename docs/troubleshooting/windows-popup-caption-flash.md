# 右键悬浮窗时闪出系统标题栏（`GameAccel 网...` / `— □ ×`）

- 影响版本：v0.3.0 – v0.3.2
- 修复版本：v0.3.3
- 平台：Windows 10/11（eframe 0.31 + winit 0.30 + glow）
- 相关代码：[`crates/gui/src/tray.rs`](../../crates/gui/src/tray.rs)（`strip_native_chrome`、`chromeless_wndproc`、`prepare_popup_window`）

## 现象

在桌面网速悬浮窗上点右键，弹出菜单的瞬间，悬浮窗上半部分会闪出一条系统标题栏：
早期是 Win11 风格的 `— □ ×` 按钮，后来变成带标题文字的经典蓝灰标题栏 `GameAccel 网...`。

## 最终根因

**系统把标题栏直接画在了悬浮窗的内容上面，跟窗口样式无关。**

1. 右键菜单弹出时会抢走焦点，悬浮窗收到 `WM_NCACTIVATE`（失去激活）。
2. winit 处理完焦点后把消息交给 `DefWindowProcW`。`DefWindowProcW` 会和 uxtheme 的
   未公开消息 `WM_NCUAHDRAWCAPTION (0xAE)` / `WM_NCUAHDRAWFRAME (0xAF)` 一起，
   按窗口标题把标题栏（标题文字 + `— □ ×`）画到窗口上。对自绘边框的窗口
   （winit 在 `WM_NCCALCSIZE` 里把客户区扩成整个窗口），这一步会直接盖在客户区上，
   而且不看窗口当前有没有 `WS_CAPTION`。
3. 下一帧 OpenGL 重绘把它盖掉，于是表现为“闪一下”。

## 修复

给悬浮窗和右键菜单挂上自定义窗口过程 `chromeless_wndproc`（`SetWindowLongPtrW(GWLP_WNDPROC)`，
原过程按 HWND 存表，`WM_NCDESTROY` 时移除）：

| 消息 | 处理 |
|---|---|
| `WM_NCACTIVATE` | 把 `lParam` 改成 `-1` 后交给原过程：winit 照常更新焦点，`DefWindowProc` 按文档约定不重画非客户区 |
| `WM_NCPAINT`、`WM_NCUAHDRAWCAPTION`、`WM_NCUAHDRAWFRAME` | 直接返回 0（这些窗口没有非客户区） |
| 其他 | 原样交给 winit |

同时保留：剥离 `WS_CAPTION / WS_SYSMENU / WS_BORDER` 改为 `WS_POPUP`、关闭 DWM 过渡动画、
右键菜单用 `WS_EX_LAYERED` 先以透明度 0 渲染首帧再切到 255。

## 排查过程中踩过的坑

按时间顺序，前几轮都没碰到真正原因：

1. **以为是子窗口首帧可见** → 改成 `with_visible(false)` 再发 `ViewportCommand::Visible(true)`。
   无效：winit 在 `Visible` 时会按自己记录的样式重写 `GWL_STYLE`，把 `WS_CAPTION` 加回来。
2. **以为是 winit 恢复样式** → 隐藏状态下自己剥离样式、用 Win32 `SetWindowPos` 显示，
   并设置 `DWMWA_NCRENDERING_POLICY = DWMNCRP_DISABLED`。
   反而更糟：关闭 DWM 非客户区渲染后，`DefWindowProc` 改用经典方式画标题栏，
   标题栏从 `— □ ×` 变成了带文字的 `GameAccel 网...`。**不要设置这个属性。**
3. **以为是 WGL 首帧白屏** → 加 `WS_EX_LAYERED` 透明预渲染。这条对菜单本身有用，但不是标题栏的原因。
4. **符号扩展 bug**：`const WS_POPUP: isize = 0x8000_0000u32 as i32 as isize` 在 64 位下是
   `0xFFFF_FFFF_8000_0000`，而 `GetWindowLongPtrW` 返回 `0x9600_0000`，比较永远不相等，
   导致每帧都 `SetWindowLongPtrW + SWP_FRAMECHANGED`（日志里约 47ms 刷一次）。
   改为 `0x8000_0000u32 as isize`。修掉后悬浮窗不再持续闪，但右键时的闪烁还在。
5. **测试时跑的是旧进程**：程序以管理员权限运行（TUN），双击新包会再拉起一个提权实例，
   旧实例不退出；两个悬浮窗叠在同一位置，右键点到的是旧的。验证前务必确认只剩一个进程、
   启动时间晚于编译时间：
   ```powershell
   Get-CimInstance Win32_Process -Filter "Name like 'game-accel%'" | Select ProcessId, CreationDate
   ```

## 定位方法（值得复用）

最后是靠一次高频采样定位的：右键按下时起一个后台线程，以约 1ms 间隔、持续 1.5 秒，
枚举本进程所有顶层窗口，把 `可见性 / GWL_STYLE / GWL_EXSTYLE / 位置尺寸 / 标题` 的每次变化写进
`%APPDATA%\game-accel\logs\app.log`。

结果显示悬浮窗样式全程是 `0x96000000`（`WS_POPUP`，无 `WS_CAPTION`），菜单也是先剥离再显示，
**没有任何窗口在任何时刻带标题栏样式**。由此排除了“样式被恢复”这条线，确定是消息处理阶段直接绘制。
诊断代码已在发版前移除。

## 经验

- winit 0.30 在 Windows 上即使 `with_decorations(false)`，窗口仍带 `WS_CAPTION | WS_SYSMENU`，
  只是靠 `WM_NCCALCSIZE` 把标题栏盖住；改样式只能治标，激活切换时的绘制要在窗口过程里拦。
- 不要设置 `DWMNCRP_DISABLED`。
- 写 Win32 样式常量时用 `u32` 或零扩展，避免 `as i32 as isize` 的符号扩展。
- 先用日志 / 采样拿到事实，再改代码；连续几轮凭推测修改都落空了。
