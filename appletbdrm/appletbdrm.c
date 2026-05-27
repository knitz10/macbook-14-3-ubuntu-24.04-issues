/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Apple Touch Bar DRM Driver
 *
 * Copyright (c) 2023-2026 Kerem Karabay <kekrby@gmail.com>
 * Copyright (c) 2025-2026 sunplex07
 *
 * T1 (2016-2017) and T2 (2018-2020) MacBook Pro Touch Bar support.
 * T1: HID Feature Reports (Interface 6, Usage Page 0xFF12)
 */

#include <linux/module.h>
#include <linux/usb.h>
#include <linux/completion.h>
#include <linux/workqueue.h>
#include <linux/delay.h>
#include <linux/align.h>
#include <linux/math.h>
#include <linux/fb.h>
#include <linux/backlight.h>

#include <drm/drm_atomic.h>
#include <drm/drm_atomic_helper.h>
#include <drm/drm_drv.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_framebuffer.h>
#include <drm/drm_gem_atomic_helper.h>
#include <drm/drm_gem_framebuffer_helper.h>
#include <drm/drm_gem_shmem_helper.h>
#include <drm/drm_plane.h>
#include <drm/drm_probe_helper.h>
#include <drm/drm_damage_helper.h>
#include <drm/drm_format_helper.h>
#include <drm/drm_print.h>

/* USB device identifiers */
#define APPLE_VID			0x05ac
#define IBRIDGE_PID			0x8600	/* T1 iBridge */
#define IBRIDGE_PID_T2			0x8302	/* T2 iBridge */
#define IBRIDGE_INTERFACE_NUM		3	/* DFR display interface */

/* T1 HID brightness control - Interface 6, Usage Page 0xFF12 */
#define T1_HID_INTERFACE_NUM		6
#define HID_REQ_GET_REPORT		0x01
#define HID_REQ_SET_REPORT		0x09
#define HID_REPORT_TYPE_FEATURE		0x03

#define T1_HID_REPORT5_ID		5	/* AutoBrightness */
#define T1_HID_REPORT5_LEN		116
#define T1_HID_AUTOBRIGHTNESS_OFF	1	/* manual */
#define T1_HID_AUTOBRIGHTNESS_ON	2	/* ALS-driven */

#define T1_HID_REPORT4_ID		4	/* absolute brightness */
#define T1_HID_REPORT4_LEN		14

#define T1_HID_REPORT3_ID		3	/* display state */
#define T1_HID_REPORT3_LEN		15
#define T1_HID_DISPLAYSTATE_OFF		1
#define T1_HID_DISPLAYSTATE_ON		2

#define T1_HID_MIN_NITS_DEFAULT		11899	/* ~12 nits */
#define T1_HID_MAX_NITS_DEFAULT		357099	/* ~357 nits */

/* Protocol opcodes (little-endian ASCII) */
#define APPLETBDRM_BITS_PER_PIXEL	24
#define APPLETBDRM_MSG_CLEAR_DISPLAY	cpu_to_le32(0x434c5244)	/* CLRD */
#define APPLETBDRM_MSG_GET_INFORMATION	cpu_to_le32(0x47494e46)	/* GINF */
#define APPLETBDRM_MSG_SIGNAL_READINESS	cpu_to_le32(0x52454459)	/* REDY */
#define APPLETBDRM_MSG_UPDATE_COMPLETE	cpu_to_le32(0x5544434c)	/* UDCL */
#define APPLETBDRM_MSG_STATS		cpu_to_le32(0x53544154)	/* STAT */
#define APPLETBDRM_MSG_SET_BRIGHTNESS	cpu_to_le32(0x4e544253)	/* SBTN T2 */
#define APPLETBDRM_MSG_WHITE_DISPLAY	cpu_to_le32(0x57445350)	/* WDSP */

#define ASYNC_MSG_MAX_SIZE		1024
#define USB_CMD_TIMEOUT			1000	/* ms - matches upstream */
#define USB_EAGAIN_RETRY_COUNT		20
#define USB_EAGAIN_RETRY_DELAY_MS	20

#define drm_to_adev(_drm)	container_of(_drm, struct appletbdrm_device, drm)

/* Inches to mm conversion factor */
#define INCHES_TO_MM			254	/* 25.4 * 10 for integer math */

struct appletbdrm_msg_request_header {
	__le16 unk_00;
	__le16 unk_02;
	__le32 unk_04;
	__le32 unk_08;
	__le32 size;
} __packed;

struct appletbdrm_msg_response_header {
	u8 unk_00[16];
	__le32 msg;
} __packed;

struct appletbdrm_msg_simple_request {
	struct appletbdrm_msg_request_header header;
	__le32 msg;
	u8 unk_14[8];
	__le32 size;
} __packed;

struct appletbdrm_msg_set_brightness {
	struct appletbdrm_msg_request_header header;
	__le32 msg;
	u8 unk_14[8];
	__le32 size;
	__le32 brightness;	/* IEEE 754 float as u32 */
	u8 unk_20[8];
} __packed;

struct appletbdrm_msg_information {
	struct appletbdrm_msg_response_header header;
	u8 unk_14[12];
	__le32 width;
	__le32 height;
	u8 bits_per_pixel;
	__le32 bytes_per_row;
	__le32 orientation;
	__le32 bitmap_info;
	__le32 pixel_format;
	__le32 width_inches;	/* IEEE 754 float */
	__le32 height_inches;	/* IEEE 754 float */
} __packed;

struct appletbdrm_frame {
	__le16 begin_x;
	__le16 begin_y;
	__le16 width;
	__le16 height;
	__le32 buf_size;
	u8 buf[];
} __packed;

struct appletbdrm_fb_request_footer {
	u8 unk_00[12];
	__le32 unk_0c;
	u8 unk_10[12];
	__le32 unk_1c;
	__le64 timestamp;
	u8 unk_28[12];
	__le32 unk_34;
	u8 unk_38[20];
	__le32 unk_4c;
} __packed;

struct appletbdrm_fb_request {
	struct appletbdrm_msg_request_header header;
	__le16 unk_10;
	u8 msg_id;
	u8 unk_13[29];
	u8 data[];
} __packed;

struct appletbdrm_fb_request_response {
	struct appletbdrm_msg_response_header header;
	u8 unk_14[12];
	__le64 timestamp;
} __packed;

enum appletbdrm_mac_type {
	MAC_TYPE_T1,
	MAC_TYPE_T2,
};

struct appletbdrm_device {
	struct device *dmadev;
	struct usb_device *udev;
	struct usb_interface *interface;	/* Interface 3 - display */
	struct usb_interface *hid_interface;	/* Interface 6 - T1 brightness */
	enum appletbdrm_mac_type mac_type;

	unsigned int in_ep;
	unsigned int out_ep;
	unsigned int width;
	unsigned int height;
	u32 width_mm;
	u32 height_mm;

	struct drm_device drm;
	struct drm_display_mode mode;
	struct drm_connector connector;
	struct drm_plane primary_plane;
	struct drm_crtc crtc;
	struct drm_encoder encoder;

	struct urb *async_in_urb;
	void *async_in_buffer;
	struct work_struct init_work;
	struct completion ginf_completion;

	struct backlight_device *bl_dev;
	int current_brightness;
	u32 min_nits;
	u32 max_nits;
	bool auto_brightness_disabled;
	bool hid_interface_claimed;
	bool backlight_initialized;

	/*
	 * Set to true only after drm_dev_register() succeeds.
	 * Guards drm_dev_unplug / drm_atomic_helper_shutdown in disconnect,
	 * which crash on an unregistered DRM device (uninitialized mode_config
	 * mutexes).  Probe sets intfdata before the DRM is ready, so disconnect
	 * can be called even when probe failed.
	 */
	bool drm_registered;
};

struct appletbdrm_plane_state {
	struct drm_shadow_plane_state base;
	struct appletbdrm_fb_request *request;
	struct appletbdrm_fb_request_response *response;
	size_t request_size;
	size_t frames_size;
};

static inline struct appletbdrm_plane_state *
to_appletbdrm_plane_state(struct drm_plane_state *state)
{
	return container_of(state, struct appletbdrm_plane_state, base.base);
}

/*
 * Integer-only IEEE 754 single-precision conversion.
 * Kernel code cannot use floating-point directly.
 */

static inline __le32 brightness_to_ieee754(int brightness)
{
	u32 ieee;
	u32 mantissa;
	int exp;

	if (brightness <= 0)
		return cpu_to_le32(0);		/* 0.0 */
	if (brightness >= 255)
		return cpu_to_le32(0x3F800000);	/* 1.0 */

	/*
	 * Compute brightness/255 as IEEE 754.
	 * Scale to fixed-point: (brightness << 24) / 255 gives 24-bit fraction.
	 * Then normalize to IEEE 754 format.
	 */
	mantissa = ((u32)brightness << 24) / 255;

	/* Find leading bit position for normalization */
	exp = 127;	/* IEEE 754 bias */
	while (!(mantissa & 0x800000) && exp > 0) {
		mantissa <<= 1;
		exp--;
	}

	/* Remove implicit leading 1, keep 23-bit mantissa */
	mantissa &= 0x7FFFFF;
	ieee = ((u32)exp << 23) | mantissa;

	return cpu_to_le32(ieee);
}

/*
 * convert float (__le32) to integer, for parsing display dimensions in inches
 */
static inline u32 ieee754_to_milli(__le32 li)
{
	u32 ieee = le32_to_cpu(li);
	u32 mantissa;
	int exp;
	u32 result;

	if (ieee == 0)
		return 0;

	exp = ((ieee >> 23) & 0xFF) - 127;
	mantissa = (ieee & 0x7FFFFF) | 0x800000;	/* Add implicit 1 */

	/*
	 * mantissa is 1.xxx in Q23 format (24 bits, implicit binary point after bit 23).
	 * Multiply by 1000, then shift based on exponent.
	 * Result = mantissa * 1000 * 2^(exp-23)
	 */
	if (exp >= 0) {
		result = (mantissa * 1000ULL) >> (23 - exp);
	} else {
		result = (mantissa * 1000ULL) >> (23 - exp);
	}

	return result;
}

/* Forward declarations */
static int appletbdrm_setup_mode_config(struct appletbdrm_device *adev);
static void appletbdrm_async_urb_complete(struct urb *urb);
static void appletbdrm_init_work_fn(struct work_struct *work);
static void appletbdrm_crtc_helper_atomic_enable(struct drm_crtc *crtc,
						 struct drm_atomic_state *state);
static void appletbdrm_crtc_helper_atomic_disable(struct drm_crtc *crtc,
						  struct drm_atomic_state *state);
static struct usb_driver appletbdrm_usb_driver;

/*
 * convert XRGB8888 to BGR888 for touch bar display.
 * BGR888 stores bytes as R,G,B in memory order.
 * from upstream drm_fb_xrgb8888_to_bgr888 implementation.
 */
static void appletbdrm_xrgb8888_to_bgr888(struct iosys_map *dst,
					  struct iosys_map *src,
					  const struct drm_rect *rect,
					  struct drm_framebuffer *fb)
{
	const u8 *src_base = src->vaddr;
	u8 *dst_base = dst->vaddr;
	unsigned int h = drm_rect_height(rect);
	unsigned int w = drm_rect_width(rect);
	unsigned int x, y;

	for (y = 0; y < h; y++) {
		const __le32 *src_line;
		u8 *dst_line;

		src_line = (const __le32 *)(src_base +
			   (rect->y1 + y) * fb->pitches[0]) + rect->x1;
		dst_line = dst_base + y * w * 3;

		for (x = 0; x < w; x++) {
			u32 pix = le32_to_cpu(src_line[x]);

			/* output R,G,B byte order (BGR888 format) */
			*dst_line++ = (pix & 0x00FF0000) >> 16;
			*dst_line++ = (pix & 0x0000FF00) >> 8;
			*dst_line++ = (pix & 0x000000FF);
		}
	}
}

static int appletbdrm_send_request(struct appletbdrm_device *adev,
				   void *request, size_t size)
{
	int actual_size;
	int ret;
	int attempt;

	for (attempt = 0; attempt < USB_EAGAIN_RETRY_COUNT; attempt++) {
		ret = usb_bulk_msg(adev->udev,
				   usb_sndbulkpipe(adev->udev, adev->out_ep),
				   request, size, &actual_size, USB_CMD_TIMEOUT);
		if (ret != -EAGAIN)
			break;
		msleep(USB_EAGAIN_RETRY_DELAY_MS);
	}

	return ret;
}

static int appletbdrm_send_simple_cmd(struct appletbdrm_device *adev, __le32 msg)
{
	struct appletbdrm_msg_simple_request *request;
	int ret;

	request = kzalloc(sizeof(*request), GFP_KERNEL);
	if (!request)
		return -ENOMEM;

	request->header.unk_00 = cpu_to_le16(2);
	request->header.unk_02 = cpu_to_le16(0x1512);
	request->header.size = cpu_to_le32(sizeof(*request) - sizeof(request->header));
	request->msg = msg;
	request->size = request->header.size;

	ret = appletbdrm_send_request(adev, request, sizeof(*request));
	kfree(request);
	return ret;
}

/*
 * T1 HID Interface Management - Brightness requires Interface 6 (DFR Brightness HID page)
 * claimed by usbhid by default; unbind and claim it for HID Feature Reports.
 */
static int appletbdrm_claim_hid_interface(struct appletbdrm_device *adev)
{
	struct usb_interface *hid_intf;
	struct usb_device *udev = adev->udev;
	int ret;

	if (adev->hid_interface_claimed)
		return 0;

	hid_intf = usb_ifnum_to_if(udev, T1_HID_INTERFACE_NUM);
	if (!hid_intf) {
		dev_err(adev->dmadev, "Interface %d not found\n",
			T1_HID_INTERFACE_NUM);
		return -ENODEV;
	}

	if (hid_intf->dev.driver) {
		dev_dbg(adev->dmadev, "Unbinding %s from interface %d\n",
			hid_intf->dev.driver->name, T1_HID_INTERFACE_NUM);
		device_release_driver(&hid_intf->dev);
	}

	ret = usb_driver_claim_interface(&appletbdrm_usb_driver, hid_intf, adev);
	if (ret) {
		dev_err(adev->dmadev, "Failed to claim interface %d: %d\n",
			T1_HID_INTERFACE_NUM, ret);
		return ret;
	}

	adev->hid_interface = hid_intf;
	adev->hid_interface_claimed = true;
	dev_dbg(adev->dmadev, "Claimed HID interface %d\n", T1_HID_INTERFACE_NUM);

	return 0;
}

static void appletbdrm_release_hid_interface(struct appletbdrm_device *adev)
{
	if (!adev->hid_interface_claimed || !adev->hid_interface)
		return;

	usb_driver_release_interface(&appletbdrm_usb_driver, adev->hid_interface);
	adev->hid_interface = NULL;
	adev->hid_interface_claimed = false;
}

/*
 * T1 HID Brightness - Usage Page 0xFF12
 *
 * Report 5: AutoBrightness (byte[3]=1 disable, =2 enable)
 * Report 4: Absolute nits (bytes[2-5])
 * Report 3: DisplayState (1=off, 2=on)
 *
 * must disable AutoBrightness before manual control takes effect.
 */
static int appletbdrm_hid_get_report(struct appletbdrm_device *adev,
				     u8 report_id, u8 *buf, size_t len)
{
	u8 *dma_buf;
	int ret;

	if (!adev->hid_interface_claimed) {
		ret = appletbdrm_claim_hid_interface(adev);
		if (ret)
			return ret;
	}

	dma_buf = kmalloc(len, GFP_KERNEL);
	if (!dma_buf)
		return -ENOMEM;

	dma_buf[0] = report_id;

	ret = usb_control_msg(adev->udev,
			      usb_rcvctrlpipe(adev->udev, 0),
			      HID_REQ_GET_REPORT,
			      USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE,
			      (HID_REPORT_TYPE_FEATURE << 8) | report_id,
			      T1_HID_INTERFACE_NUM,
			      dma_buf, len, USB_CMD_TIMEOUT);

	if (ret > 0)
		memcpy(buf, dma_buf, ret);

	kfree(dma_buf);
	return ret;
}

static int appletbdrm_hid_set_report(struct appletbdrm_device *adev,
				     u8 report_id, u8 *buf, size_t len)
{
	u8 *dma_buf;
	int ret;

	if (!adev->hid_interface_claimed) {
		ret = appletbdrm_claim_hid_interface(adev);
		if (ret)
			return ret;
	}

	dma_buf = kmalloc(len, GFP_KERNEL);
	if (!dma_buf)
		return -ENOMEM;

	memcpy(dma_buf, buf, len);

	ret = usb_control_msg(adev->udev,
			      usb_sndctrlpipe(adev->udev, 0),
			      HID_REQ_SET_REPORT,
			      USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_INTERFACE,
			      (HID_REPORT_TYPE_FEATURE << 8) | report_id,
			      T1_HID_INTERFACE_NUM,
			      dma_buf, len, USB_CMD_TIMEOUT);

	kfree(dma_buf);
	return ret;
}

/* Read T1 brightness range from Report 5 */
static int appletbdrm_t1_read_brightness_caps(struct appletbdrm_device *adev)
{
	u8 report[T1_HID_REPORT5_LEN];
	int ret;

	memset(report, 0, sizeof(report));
	report[0] = T1_HID_REPORT5_ID;

	ret = appletbdrm_hid_get_report(adev, T1_HID_REPORT5_ID,
					report, sizeof(report));
	if (ret < 0) {
		dev_warn(adev->dmadev, "Report 5 read failed: %d\n", ret);
		adev->min_nits = T1_HID_MIN_NITS_DEFAULT;
		adev->max_nits = T1_HID_MAX_NITS_DEFAULT;
		return ret;
	}

	/* bytes[4-7]: MinNits, bytes[8-11]: MaxNits (32-bit LE) */
	adev->min_nits = report[4] | (report[5] << 8) |
			 (report[6] << 16) | (report[7] << 24);
	adev->max_nits = report[8] | (report[9] << 8) |
			 (report[10] << 16) | (report[11] << 24);

	dev_dbg(adev->dmadev, "T1 nits: %u-%u, auto=%d\n",
		adev->min_nits, adev->max_nits, report[3]);

	return 0;
}

/* Disable T1 AutoBrightness - required before manual control */
static int appletbdrm_t1_disable_autobrightness(struct appletbdrm_device *adev)
{
	u8 report[T1_HID_REPORT5_LEN];
	int ret;

	if (adev->auto_brightness_disabled)
		return 0;

	memset(report, 0, sizeof(report));
	report[0] = T1_HID_REPORT5_ID;

	ret = appletbdrm_hid_get_report(adev, T1_HID_REPORT5_ID,
					report, sizeof(report));
	if (ret < 0) {
		dev_warn(adev->dmadev, "Report 5 read failed: %d\n", ret);
		return ret;
	}
	report[3] = T1_HID_AUTOBRIGHTNESS_OFF;

	ret = appletbdrm_hid_set_report(adev, T1_HID_REPORT5_ID,
					report, sizeof(report));
	if (ret < 0) {
		dev_warn(adev->dmadev, "AutoBrightness disable failed: %d\n", ret);
		return ret;
	}

	adev->auto_brightness_disabled = true;
	dev_dbg(adev->dmadev, "T1 AutoBrightness disabled\n");
	return 0;
}

/* Set T1 brightness via Report 4 (nits value) */
static int appletbdrm_t1_set_brightness_nits(struct appletbdrm_device *adev,
					     u32 nits)
{
	u8 report[T1_HID_REPORT4_LEN];
	int ret;

	ret = appletbdrm_t1_disable_autobrightness(adev);
	if (ret < 0)
		dev_dbg(adev->dmadev, "AutoBrightness disable failed\n");

	memset(report, 0, sizeof(report));
	report[0] = T1_HID_REPORT4_ID;
	report[1] = 2;
	report[2] = (nits >> 0) & 0xFF;
	report[3] = (nits >> 8) & 0xFF;
	report[4] = (nits >> 16) & 0xFF;
	report[5] = (nits >> 24) & 0xFF;

	ret = appletbdrm_hid_set_report(adev, T1_HID_REPORT4_ID,
					report, sizeof(report));
	if (ret < 0) {
		dev_warn(adev->dmadev, "Brightness set failed: %d\n", ret);
		return ret;
	}

	dev_dbg(adev->dmadev, "T1 brightness: %u nits\n", nits);
	return 0;
}

/* Set T1 display state via Report 3 */
static int appletbdrm_t1_set_display_state(struct appletbdrm_device *adev,
					   int state)
{
	u8 report[T1_HID_REPORT3_LEN];
	int ret;

	memset(report, 0, sizeof(report));
	report[0] = T1_HID_REPORT3_ID;
	report[1] = state;
	report[6] = 0x01;

	ret = appletbdrm_hid_set_report(adev, T1_HID_REPORT3_ID,
					report, sizeof(report));
	if (ret < 0) {
		dev_warn(adev->dmadev, "Display state set failed: %d\n", ret);
		return ret;
	}

	dev_dbg(adev->dmadev, "T1 display state: %d\n", state);
	return 0;
}

/* T2 SBTN brightness - bulk transfer */
static int appletbdrm_t2_set_brightness(struct appletbdrm_device *adev,
					int brightness)
{
	struct appletbdrm_msg_set_brightness *request;
	int ret;

	request = kzalloc(sizeof(*request), GFP_KERNEL);
	if (!request)
		return -ENOMEM;

	request->header.unk_00 = cpu_to_le16(2);
	request->header.unk_02 = cpu_to_le16(0x1512);
	request->header.size = cpu_to_le32(sizeof(*request) - sizeof(request->header));
	request->msg = APPLETBDRM_MSG_SET_BRIGHTNESS;
	request->size = request->header.size;
	request->brightness = brightness_to_ieee754(brightness);

	ret = appletbdrm_send_request(adev, request, sizeof(*request));
	if (ret)
		dev_warn(adev->dmadev, "T2 SBTN failed: %d\n", ret);
	else
		dev_dbg(adev->dmadev, "T2 brightness: %d\n", brightness);

	kfree(request);
	return ret;
}

static int appletbdrm_submit_async_urb(struct appletbdrm_device *adev)
{
	int ret;
	int attempt;

	adev->async_in_urb = usb_alloc_urb(0, GFP_KERNEL);
	if (!adev->async_in_urb)
		return -ENOMEM;
	adev->async_in_buffer = kmalloc(ASYNC_MSG_MAX_SIZE, GFP_KERNEL);
	if (!adev->async_in_buffer) {
		usb_free_urb(adev->async_in_urb);
		return -ENOMEM;
	}

	usb_fill_bulk_urb(adev->async_in_urb, adev->udev,
			  usb_rcvbulkpipe(adev->udev, adev->in_ep),
			  adev->async_in_buffer, ASYNC_MSG_MAX_SIZE,
			  appletbdrm_async_urb_complete, adev);
	for (attempt = 0; attempt < 10; attempt++) {
		ret = usb_submit_urb(adev->async_in_urb, GFP_KERNEL);
		if (ret != -EAGAIN)
			break;
		msleep(20);
	}
	if (ret) {
		dev_err(adev->dmadev, "Async URB submit failed: %d\n", ret);
		kfree(adev->async_in_buffer);
		usb_free_urb(adev->async_in_urb);
		adev->async_in_urb = NULL;
	}

	return ret;
}

static int appletbdrm_parse_ginf(struct appletbdrm_device *adev,
				 struct appletbdrm_msg_information *info,
				 u32 len)
{
	u32 width_milli, height_milli;

	if (len < sizeof(*info)) {
		dev_err(adev->dmadev, "GINF too short: %u\n", len);
		return -EINVAL;
	}

	adev->width = le32_to_cpu(info->width);
	adev->height = le32_to_cpu(info->height);

	/* Convert IEEE 754 inches to milli-inches, then to mm */
	width_milli = ieee754_to_milli(info->width_inches);
	height_milli = ieee754_to_milli(info->height_inches);

	/* Swap dimensions due to 90-degree rotation, convert to mm */
	/* milli-inches * 25.4 / 1000 = mm, simplified: * 254 / 10000 */
	adev->width_mm = (height_milli * 254 + 5000) / 10000;
	adev->height_mm = (width_milli * 254 + 5000) / 10000;

	dev_info(adev->dmadev, "GINF: pixel %ux%u (mm %ux%u), DRM mode will be %ux%u\n",
		 adev->width, adev->height, adev->width_mm, adev->height_mm,
		 adev->height, adev->width);

	return 0;
}

static void appletbdrm_async_urb_complete(struct urb *urb)
{
	struct appletbdrm_device *adev = urb->context;
	struct appletbdrm_msg_response_header *resp;
	int status = urb->status;

	if (status) {
		if (status != -ENOENT && status != -ECONNRESET &&
		    status != -ESHUTDOWN)
			dev_err(adev->dmadev, "Async URB failed: %d\n", status);
		return;
	}

	if (urb->actual_length >= sizeof(*resp)) {
		resp = urb->transfer_buffer;
		dev_dbg(adev->dmadev, "Async: %p4cc len=%d\n",
			&resp->msg, urb->actual_length);

		switch (resp->msg) {
		case APPLETBDRM_MSG_STATS:
			schedule_work(&adev->init_work);
			break;
		case APPLETBDRM_MSG_GET_INFORMATION:
			if (!appletbdrm_parse_ginf(adev, urb->transfer_buffer,
						   urb->actual_length))
				complete(&adev->ginf_completion);
			break;
		case APPLETBDRM_MSG_SIGNAL_READINESS:
		case APPLETBDRM_MSG_UPDATE_COMPLETE:
			break;
		default:
			dev_dbg(adev->dmadev, "Unknown packet: %p4cc\n",
				&resp->msg);
			break;
		}
	}

	if (status != -ESHUTDOWN && status != -ENOENT)
		usb_submit_urb(urb, GFP_ATOMIC);
}

static int appletbdrm_bl_update_status(struct backlight_device *bl)
{
	struct appletbdrm_device *adev = bl_get_data(bl);
	int new_brightness = bl->props.brightness;
	u32 nits;
	int ret;

	/* Skip first call to preserve auto-brightness */
	if (!adev->backlight_initialized) {
		adev->backlight_initialized = true;
		adev->current_brightness = new_brightness;
		return 0;
	}

	if (new_brightness == adev->current_brightness &&
	    bl->props.power == FB_BLANK_UNBLANK)
		return 0;

	adev->current_brightness = new_brightness;

	if (adev->mac_type == MAC_TYPE_T1) {
		if (new_brightness == 0 || bl->props.power != FB_BLANK_UNBLANK)
			return appletbdrm_t1_set_display_state(adev,
					T1_HID_DISPLAYSTATE_OFF);

		ret = appletbdrm_t1_set_display_state(adev, T1_HID_DISPLAYSTATE_ON);
		if (ret < 0)
			return ret;

		nits = adev->min_nits +
		       ((u64)(adev->max_nits - adev->min_nits) * new_brightness) /
		       bl->props.max_brightness;

		return appletbdrm_t1_set_brightness_nits(adev, nits);
	}

	return appletbdrm_t2_set_brightness(adev, new_brightness);
}

static int appletbdrm_bl_get_brightness(struct backlight_device *bl)
{
	struct appletbdrm_device *adev = bl_get_data(bl);

	return adev->current_brightness;
}

static const struct backlight_ops appletbdrm_backlight_ops = {
	.options = BL_CORE_SUSPENDRESUME,
	.update_status = appletbdrm_bl_update_status,
	.get_brightness = appletbdrm_bl_get_brightness,
};

static void appletbdrm_register_backlight(struct appletbdrm_device *adev)
{
	struct backlight_properties props;

	if (adev->mac_type == MAC_TYPE_T1)
		appletbdrm_t1_read_brightness_caps(adev);
	else
		adev->max_nits = 255;

	memset(&props, 0, sizeof(props));
	props.type = BACKLIGHT_RAW;
	props.max_brightness = 255;
	props.brightness = 255;

	adev->backlight_initialized = false;
	adev->auto_brightness_disabled = false;
	adev->current_brightness = props.brightness;

	adev->bl_dev = devm_backlight_device_register(adev->dmadev,
						      "appletb_backlight",
						      adev->dmadev, adev,
						      &appletbdrm_backlight_ops,
						      &props);
	if (IS_ERR(adev->bl_dev)) {
		dev_err(adev->dmadev, "Backlight register failed\n");
		return;
	}

	dev_dbg(adev->dmadev, "Backlight registered\n");
}

static void appletbdrm_init_work_fn(struct work_struct *work)
{
	struct appletbdrm_device *adev =
		container_of(work, struct appletbdrm_device, init_work);
	struct drm_device *drm = &adev->drm;
	long ret;

	ret = appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_GET_INFORMATION);
	if (ret) {
		dev_err(adev->dmadev, "GINF send failed: %ld\n", ret);
		return;
	}

	ret = wait_for_completion_timeout(&adev->ginf_completion,
					  msecs_to_jiffies(USB_CMD_TIMEOUT));
	if (!ret) {
		dev_err(adev->dmadev, "GINF timeout\n");
		return;
	}

	ret = appletbdrm_setup_mode_config(adev);
	if (ret) {
		dev_err(adev->dmadev, "Mode config failed: %ld\n", ret);
		return;
	}

	/*
	 * T1: Backlight controlled via HID Feature Reports on Interface 6.
	 * T2: Backlight is a separate USB device (PID 0x8102) handled by
	 *     hid_appletb_bl driver - do not register our own.
	 */
	if (adev->mac_type == MAC_TYPE_T1)
		appletbdrm_register_backlight(adev);

	ret = drm_dev_register(drm, 0);
	if (ret) {
		dev_err(adev->dmadev, "DRM register failed: %ld\n", ret);
		return;
	}
	adev->drm_registered = true;

	dev_info(adev->dmadev, "Touch Bar initialized\n");
}

static int appletbdrm_read_response(struct appletbdrm_device *adev, void *response, size_t size)
{
	struct appletbdrm_msg_response_header *header = response;
	bool readiness_signal_received = false;
	int ret, actual_size;
	int attempt;

retry:
	for (attempt = 0; attempt < USB_EAGAIN_RETRY_COUNT; attempt++) {
		ret = usb_bulk_msg(adev->udev,
				   usb_rcvbulkpipe(adev->udev, adev->in_ep),
				   response, size, &actual_size, USB_CMD_TIMEOUT);
		if (ret != -EAGAIN)
			break;
		msleep(USB_EAGAIN_RETRY_DELAY_MS);
	}
	if (ret)
		return ret;

	/*
	 * The device may respond with a readiness signal during operation.
	 * In that case, retry to get the actual response.
	 */
	if (header->msg == APPLETBDRM_MSG_SIGNAL_READINESS) {
		if (!readiness_signal_received) {
			readiness_signal_received = true;
			goto retry;
		}
		dev_warn(adev->dmadev, "Unexpected readiness signal\n");
		return -EINTR;
	}

	if (actual_size != size)
		return -EIO;

	return 0;
}

/*
 * T1 synchronous probe.
 *
 * The T1 device sends a spontaneous STATS packet to signal it is ready before
 * it will respond to a GINF request.  The original driver used an async URB to
 * wait for this packet, but that path fails on this hardware with -EAGAIN due
 * to xHCI DMA constraints.
 *
 * Strategy:
 *   1. Poll the bulk-IN endpoint with a short timeout to drain the STATS packet.
 *   2. Send GINF; after GINF the device may send an interleaved REDY or STATS
 *      before the actual GINF response, so retry on those.
 *   3. Continue with normal DRM setup.
 *
 * All receive buffers are kmalloc'd (never stack-allocated) to avoid the
 * CONFIG_VMAP_STACK xHCI DMA rejection that triggers -EAGAIN.
 */
static int appletbdrm_probe_t1_sync(struct appletbdrm_device *adev)
{
	struct appletbdrm_msg_response_header *drain;
	struct appletbdrm_msg_information *info;
	struct drm_device *drm = &adev->drm;
	int actual_size, ret, attempt;

	dev_info(adev->dmadev, "T1 synchronous init\n");

	/*
	 * Clear any stalled/halted state on the bulk endpoints left over from
	 * previous failed probe attempts.  Without this, usb_bulk_msg returns
	 * -EAGAIN/-EPIPE immediately even though the device is physically present.
	 */
	usb_clear_halt(adev->udev, usb_sndbulkpipe(adev->udev, adev->out_ep));
	usb_clear_halt(adev->udev, usb_rcvbulkpipe(adev->udev, adev->in_ep));

	/*
	 * Send CLRD to reset the device display state.  If the device was
	 * previously initialized (e.g. driver was unloaded and reloaded without
	 * a USB disconnect), it will be in a "display running" state and won't
	 * spontaneously send a new STATS packet.  CLRD nudges it back toward
	 * the initial state so the GINF exchange can proceed cleanly.
	 * Ignore any error here - the device may not respond if cold.
	 */
	appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_CLEAR_DISPLAY);

	/* Drain incoming packets looking for STATS (device-ready signal).
	 * Unknown/short packets are drained too - the device sometimes sends
	 * a 16-byte "endpoint ready" indication after usb_clear_halt or CLRD.
	 */
	drain = kmalloc(ASYNC_MSG_MAX_SIZE, GFP_KERNEL);
	if (!drain)
		return -ENOMEM;

	for (attempt = 0; attempt < 6; attempt++) {
		ret = usb_bulk_msg(adev->udev,
				   usb_rcvbulkpipe(adev->udev, adev->in_ep),
				   drain, ASYNC_MSG_MAX_SIZE,
				   &actual_size, 400);
		if (ret == -ETIMEDOUT || ret == -ETIME) {
			dev_dbg(adev->dmadev, "T1 drain: timeout after %d tries\n",
				attempt);
			break;
		}
		if (ret < 0) {
			dev_dbg(adev->dmadev, "T1 drain: ret=%d attempt=%d\n",
				ret, attempt);
			if (ret == -EAGAIN)
				continue;	/* endpoint clearing, retry */
			break;
		}
		dev_dbg(adev->dmadev, "T1 drain: got %p4cc len=%d\n",
			&drain->msg, actual_size);
		if (drain->msg == APPLETBDRM_MSG_STATS) {
			dev_dbg(adev->dmadev, "T1: got STATS, device ready\n");
			break;
		}
		/* Unknown/short packet - drain and continue */
	}
	kfree(drain);

	/* Send GINF request */
	info = kzalloc(sizeof(*info), GFP_KERNEL);
	if (!info)
		return -ENOMEM;

	ret = appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_GET_INFORMATION);
	if (ret) {
		dev_err(adev->dmadev, "T1 GINF send failed: %d\n", ret);
		goto free_info;
	}

	/*
	 * Read GINF response.  T1 may interleave REDY, STATS, or short unknown
	 * packets before the actual GINF response; drain them and retry.
	 * Every 3 unknown packets we re-send the GINF request in case the device
	 * missed it (happens after a warm re-probe without a USB disconnect).
	 */
	for (attempt = 0; attempt < 12; attempt++) {
		int try;

		/* Periodically resend GINF in case device missed it. */
		if (attempt > 0 && attempt % 3 == 0) {
			dev_dbg(adev->dmadev, "T1 GINF: resending request (attempt %d)\n",
				attempt);
			appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_GET_INFORMATION);
		}

		memset(info, 0, sizeof(*info));
		actual_size = 0;

		for (try = 0; try < 5; try++) {
			ret = usb_bulk_msg(adev->udev,
					   usb_rcvbulkpipe(adev->udev, adev->in_ep),
					   info, sizeof(*info),
					   &actual_size, USB_CMD_TIMEOUT);
			if (ret != -EAGAIN)
				break;
			msleep(USB_EAGAIN_RETRY_DELAY_MS);
		}
		if (ret == -ETIMEDOUT || ret == -ETIME) {
			dev_dbg(adev->dmadev, "T1 GINF: read timeout attempt=%d\n",
				attempt);
			break;
		}
		if (ret) {
			dev_dbg(adev->dmadev, "T1 GINF read error %d attempt=%d, continuing\n",
				ret, attempt);
			continue;
		}

		if (info->header.msg == APPLETBDRM_MSG_GET_INFORMATION)
			break;

		/* Known interleaved packets - drain and continue. */
		if (info->header.msg == APPLETBDRM_MSG_SIGNAL_READINESS ||
		    info->header.msg == APPLETBDRM_MSG_STATS) {
			dev_dbg(adev->dmadev,
				"T1 GINF: skipping interleaved %p4cc\n",
				&info->header.msg);
			continue;
		}

		/* Unknown/short packet - drain it and retry rather than fail. */
		dev_dbg(adev->dmadev,
			"T1 GINF: draining unknown %p4cc (size=%d), attempt=%d\n",
			&info->header.msg, actual_size, attempt);
	}

	if (info->header.msg != APPLETBDRM_MSG_GET_INFORMATION) {
		dev_err(adev->dmadev, "T1 GINF: no valid response after %d attempts (last=%p4cc size=%d)\n",
			attempt, &info->header.msg, actual_size);
		ret = -ETIMEDOUT;
		goto free_info;
	}

	if ((size_t)actual_size < sizeof(*info)) {
		dev_err(adev->dmadev, "T1 GINF too short: %d\n", actual_size);
		ret = -EIO;
		goto free_info;
	}

	ret = appletbdrm_parse_ginf(adev, info, actual_size);
	if (ret)
		goto free_info;

	kfree(info);

	/* Signal readiness back to device */
	ret = appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_SIGNAL_READINESS);
	if (ret) {
		dev_err(adev->dmadev, "T1 REDY send failed: %d\n", ret);
		return ret;
	}

	ret = appletbdrm_setup_mode_config(adev);
	if (ret) {
		dev_err(adev->dmadev, "T1 mode config failed: %d\n", ret);
		return ret;
	}

	appletbdrm_register_backlight(adev);

	ret = drm_dev_register(drm, 0);
	if (ret) {
		dev_err(adev->dmadev, "T1 DRM register failed: %d\n", ret);
		return ret;
	}
	adev->drm_registered = true;

	ret = appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_CLEAR_DISPLAY);
	if (ret)
		dev_warn(adev->dmadev, "T1 clear display failed: %d\n", ret);

	dev_info(adev->dmadev, "Touch Bar T1 initialized\n");
	return 0;

free_info:
	kfree(info);
	return ret;
}

/*
 * Synchronous T2 probe - matches upstream behavior.
 * No async URBs to avoid USB bandwidth contention with HID multitouch.
 */
static int appletbdrm_probe_t2_sync(struct appletbdrm_device *adev)
{
	struct appletbdrm_msg_information *info;
	struct drm_device *drm = &adev->drm;
	int ret;

	info = kzalloc(sizeof(*info), GFP_KERNEL);
	if (!info)
		return -ENOMEM;

	/* Send GINF request */
	ret = appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_GET_INFORMATION);
	if (ret) {
		dev_err(adev->dmadev, "GINF send failed: %d\n", ret);
		goto free_info;
	}

	/* Read GINF response synchronously */
	ret = appletbdrm_read_response(adev, info, sizeof(*info));
	if (ret) {
		dev_err(adev->dmadev, "GINF read failed: %d\n", ret);
		goto free_info;
	}

	if (info->header.msg != APPLETBDRM_MSG_GET_INFORMATION) {
		dev_err(adev->dmadev, "Unexpected response: %p4cc\n",
			&info->header.msg);
		ret = -EIO;
		goto free_info;
	}

	/* Parse display information */
	ret = appletbdrm_parse_ginf(adev, info, sizeof(*info));
	if (ret)
		goto free_info;

	kfree(info);

	/* Signal readiness */
	ret = appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_SIGNAL_READINESS);
	if (ret) {
		dev_err(adev->dmadev, "REDY send failed: %d\n", ret);
		return ret;
	}

	/* Setup DRM mode config */
	ret = appletbdrm_setup_mode_config(adev);
	if (ret) {
		dev_err(adev->dmadev, "Mode config failed: %d\n", ret);
		return ret;
	}

	/* Register DRM device */
	ret = drm_dev_register(drm, 0);
	if (ret) {
		dev_err(adev->dmadev, "DRM register failed: %d\n", ret);
		return ret;
	}
	adev->drm_registered = true;

	/* Clear display */
	ret = appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_CLEAR_DISPLAY);
	if (ret)
		dev_warn(adev->dmadev, "Clear display failed: %d\n", ret);

	dev_info(adev->dmadev, "Touch Bar initialized\n");
	return 0;

free_info:
	kfree(info);
	return ret;
}

static int appletbdrm_connector_helper_get_modes(struct drm_connector *connector)
{
	struct appletbdrm_device *adev = drm_to_adev(connector->dev);
	return drm_connector_helper_get_modes_fixed(connector, &adev->mode);
}

static const u32 appletbdrm_primary_plane_formats[] = {
	DRM_FORMAT_BGR888,
	DRM_FORMAT_XRGB8888, /* emulated */
};

static int appletbdrm_flush_damage(struct appletbdrm_device *adev,
				   struct drm_plane_state *old_state,
				   struct drm_plane_state *state)
{
	struct appletbdrm_plane_state *appletbdrm_state = to_appletbdrm_plane_state(state);
	struct drm_shadow_plane_state *shadow_plane_state = to_drm_shadow_plane_state(state);
	struct appletbdrm_fb_request_response *response = appletbdrm_state->response;
	struct appletbdrm_fb_request_footer *footer;
	struct drm_atomic_helper_damage_iter iter;
	struct drm_framebuffer *fb = state->fb;
	struct appletbdrm_fb_request *request = appletbdrm_state->request;
	struct drm_device *drm = &adev->drm;
	struct appletbdrm_frame *frame;
	u64 timestamp = ktime_get_ns();
	struct drm_rect damage;
	size_t frames_size = appletbdrm_state->frames_size;
	size_t request_size = appletbdrm_state->request_size;
	int ret;

	if (!frames_size)
		return 0;

	ret = drm_gem_fb_begin_cpu_access(fb, DMA_FROM_DEVICE);
	if (ret) {
		drm_err(drm, "Failed to start CPU framebuffer access (%d)\n", ret);
		goto end_fb_cpu_access;
	}

	request->header.unk_00 = cpu_to_le16(2);
	request->header.unk_02 = cpu_to_le16(0x12);
	request->header.unk_04 = cpu_to_le32(9);
	request->header.size = cpu_to_le32(request_size - sizeof(request->header));
	request->unk_10 = cpu_to_le16(1);
	request->msg_id = timestamp;

	frame = (struct appletbdrm_frame *)request->data;

	drm_atomic_helper_damage_iter_init(&iter, old_state, state);
	drm_atomic_for_each_plane_damage(&iter, &damage) {
		struct drm_rect dst_clip = state->dst;
		struct iosys_map dst = IOSYS_MAP_INIT_VADDR(frame->buf);
		u32 buf_size = drm_rect_width(&damage) * drm_rect_height(&damage) *
			       BITS_TO_BYTES(APPLETBDRM_BITS_PER_PIXEL);

		if (!drm_rect_intersect(&dst_clip, &damage))
			continue;

		frame->begin_x = cpu_to_le16(damage.y1);
		/*
		 * begin_y is the physical Y start of the damage rect in the
		 * rotated (portrait) panel coordinate system.  The logical X
		 * axis maps to the physical Y axis (RIGHT_UP rotation), so
		 * begin_y = long_side - damage.x2.
		 * For T2: long_side == adev->height (device reports portrait).
		 * For T1: long_side == adev->width  (device reports landscape).
		 * Use max() to get the correct long-side dimension for both.
		 */
		/*
		 * In portrait DRM mode (hdisplay=adev->height, vdisplay=adev->width),
		 * damage.x2 ≤ hdisplay = adev->height, so no underflow here.
		 */
		frame->begin_y = cpu_to_le16(adev->height - damage.x2);
		frame->width = cpu_to_le16(drm_rect_height(&damage));
		frame->height = cpu_to_le16(drm_rect_width(&damage));
		frame->buf_size = cpu_to_le32(buf_size);

		switch (fb->format->format) {
		case DRM_FORMAT_XRGB8888:
			appletbdrm_xrgb8888_to_bgr888(&dst,
						      &shadow_plane_state->data[0],
						      &damage, fb);
			break;
		default:
			drm_fb_memcpy(&dst, NULL, &shadow_plane_state->data[0],
				      fb, &damage);
			break;
		}
		frame = (void *)frame + struct_size(frame, buf, buf_size);
	}

	footer = (struct appletbdrm_fb_request_footer *)&request->data[frames_size];
	footer->unk_0c = cpu_to_le32(0xfffe);
	footer->unk_1c = cpu_to_le32(0x80001);
	footer->unk_34 = cpu_to_le32(0x80002);
	footer->unk_4c = cpu_to_le32(0xffff);
	footer->timestamp = cpu_to_le64(timestamp);

	ret = appletbdrm_send_request(adev, request, request_size);
	if (ret)
		goto end_fb_cpu_access;

	ret = appletbdrm_read_response(adev, response, sizeof(*response));
	if (ret)
		goto end_fb_cpu_access;

	if (response->header.msg != APPLETBDRM_MSG_UPDATE_COMPLETE) {
		drm_err(drm, "Invalid framebuffer response: msg=%p4cc\n",
			&response->header.msg);
		ret = -EIO;
	} else if (response->timestamp != footer->timestamp &&
		   response->timestamp != cpu_to_le64(U64_MAX)) {
		/*
		 * T2 echoes back the request timestamp; T1 returns 0xFFFFFFFF…
		 * (U64_MAX) as a sentinel.  Only error on an unexpected mismatch.
		 */
		drm_err(drm,
			"Framebuffer timestamp mismatch: got=0x%016llx want=0x%016llx\n",
			(unsigned long long)le64_to_cpu(response->timestamp),
			(unsigned long long)le64_to_cpu(footer->timestamp));
		ret = -EIO;
	}

end_fb_cpu_access:
	drm_gem_fb_end_cpu_access(fb, DMA_FROM_DEVICE);
	return ret;
}

static int appletbdrm_primary_plane_helper_atomic_check(struct drm_plane *plane,
							struct drm_atomic_state *state)
{
	struct drm_plane_state *new_plane_state = drm_atomic_get_new_plane_state(state, plane);
	struct drm_plane_state *old_plane_state = drm_atomic_get_old_plane_state(state, plane);
	struct drm_crtc *new_crtc = new_plane_state->crtc;
	struct drm_crtc_state *new_crtc_state = NULL;
	struct appletbdrm_plane_state *appletbdrm_state = to_appletbdrm_plane_state(new_plane_state);
	struct drm_atomic_helper_damage_iter iter;
	struct drm_rect damage;
	size_t frames_size = 0;
	size_t request_size;
	int ret;

	if (new_crtc)
		new_crtc_state = drm_atomic_get_new_crtc_state(state, new_crtc);

	ret = drm_atomic_helper_check_plane_state(new_plane_state, new_crtc_state,
						  DRM_PLANE_NO_SCALING,
						  DRM_PLANE_NO_SCALING,
						  false, false);
	if (ret)
		return ret;
	else if (!new_plane_state->visible)
		return 0;

	drm_atomic_helper_damage_iter_init(&iter, old_plane_state, new_plane_state);
	drm_atomic_for_each_plane_damage(&iter, &damage) {
		frames_size += struct_size((struct appletbdrm_frame *)0, buf,
					   drm_rect_width(&damage) * drm_rect_height(&damage) *
					   BITS_TO_BYTES(APPLETBDRM_BITS_PER_PIXEL));
	}

	if (!frames_size)
		return 0;

	request_size = ALIGN(sizeof(struct appletbdrm_fb_request) + frames_size +
			     sizeof(struct appletbdrm_fb_request_footer), 16);

	appletbdrm_state->request = kzalloc(request_size, GFP_KERNEL);
	if (!appletbdrm_state->request)
		return -ENOMEM;

	appletbdrm_state->response = kzalloc(sizeof(*appletbdrm_state->response), GFP_KERNEL);
	if (!appletbdrm_state->response) {
		kfree(appletbdrm_state->request);
		appletbdrm_state->request = NULL;
		return -ENOMEM;
	}

	appletbdrm_state->request_size = request_size;
	appletbdrm_state->frames_size = frames_size;

	return 0;
}

static void appletbdrm_primary_plane_helper_atomic_update(struct drm_plane *plane,
							  struct drm_atomic_state *old_state)
{
	struct appletbdrm_device *adev = drm_to_adev(plane->dev);
	struct drm_device *drm = plane->dev;
	struct drm_plane_state *plane_state = plane->state;
	struct drm_plane_state *old_plane_state = drm_atomic_get_old_plane_state(old_state, plane);
	int idx;

	if (!drm_dev_enter(drm, &idx))
		return;

	appletbdrm_flush_damage(adev, old_plane_state, plane_state);

	drm_dev_exit(idx);
}

static void appletbdrm_primary_plane_helper_atomic_disable(struct drm_plane *plane,
							   struct drm_atomic_state *state)
{
	struct drm_device *dev = plane->dev;
	struct appletbdrm_device *adev = drm_to_adev(dev);
	int idx;

	if (!drm_dev_enter(dev, &idx))
		return;

	appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_CLEAR_DISPLAY);

	drm_dev_exit(idx);
}

static void appletbdrm_primary_plane_reset(struct drm_plane *plane)
{
	struct appletbdrm_plane_state *appletbdrm_state;

	if (plane->state)
		plane->funcs->atomic_destroy_state(plane, plane->state);

	appletbdrm_state = kzalloc(sizeof(*appletbdrm_state), GFP_KERNEL);
	if (!appletbdrm_state)
		return;

	__drm_gem_reset_shadow_plane(plane, &appletbdrm_state->base);
}

static struct drm_plane_state *appletbdrm_primary_plane_duplicate_state(struct drm_plane *plane)
{
	struct appletbdrm_plane_state *appletbdrm_state;

	if (WARN_ON(!plane->state))
		return NULL;

	appletbdrm_state = kzalloc(sizeof(*appletbdrm_state), GFP_KERNEL);
	if (!appletbdrm_state)
		return NULL;

	__drm_gem_duplicate_shadow_plane_state(plane, &appletbdrm_state->base);
	return &appletbdrm_state->base.base;
}

static void appletbdrm_primary_plane_destroy_state(struct drm_plane *plane,
						   struct drm_plane_state *state)
{
	struct appletbdrm_plane_state *appletbdrm_state = to_appletbdrm_plane_state(state);
	kfree(appletbdrm_state->request);
	kfree(appletbdrm_state->response);
	__drm_gem_destroy_shadow_plane_state(&appletbdrm_state->base);
	kfree(appletbdrm_state);
}

static const struct drm_plane_helper_funcs appletbdrm_primary_plane_helper_funcs = {
	DRM_GEM_SHADOW_PLANE_HELPER_FUNCS,
	.atomic_check = appletbdrm_primary_plane_helper_atomic_check,
	.atomic_update = appletbdrm_primary_plane_helper_atomic_update,
	.atomic_disable = appletbdrm_primary_plane_helper_atomic_disable,
};

static const struct drm_plane_funcs appletbdrm_primary_plane_funcs = {
	.update_plane		= drm_atomic_helper_update_plane,
	.disable_plane		= drm_atomic_helper_disable_plane,
	.reset			= appletbdrm_primary_plane_reset,
	.atomic_duplicate_state	= appletbdrm_primary_plane_duplicate_state,
	.atomic_destroy_state	= appletbdrm_primary_plane_destroy_state,
	.destroy		= drm_plane_cleanup,
};

static const struct drm_mode_config_funcs appletbdrm_mode_config_funcs = {
	.fb_create	= drm_gem_fb_create_with_dirty,
	.atomic_check	= drm_atomic_helper_check,
	.atomic_commit	= drm_atomic_helper_commit,
};

static enum drm_connector_status
appletbdrm_connector_detect(struct drm_connector *connector, bool force)
{
	return connector_status_connected;
}

static const struct drm_connector_funcs appletbdrm_connector_funcs = {
	.reset			= drm_atomic_helper_connector_reset,
	.detect			= appletbdrm_connector_detect,
	.destroy		= drm_connector_cleanup,
	.fill_modes		= drm_helper_probe_single_connector_modes,
	.atomic_destroy_state	= drm_atomic_helper_connector_destroy_state,
	.atomic_duplicate_state	= drm_atomic_helper_connector_duplicate_state,
};

static const struct drm_connector_helper_funcs appletbdrm_connector_helper_funcs = {
	.get_modes = appletbdrm_connector_helper_get_modes,
};

static enum drm_mode_status
appletbdrm_crtc_helper_mode_valid(struct drm_crtc *crtc,
				  const struct drm_display_mode *mode)
{
	struct appletbdrm_device *adev = drm_to_adev(crtc->dev);

	return drm_crtc_helper_mode_valid_fixed(crtc, mode, &adev->mode);
}

static void appletbdrm_crtc_helper_atomic_enable(struct drm_crtc *crtc,
						 struct drm_atomic_state *state)
{
	struct appletbdrm_device *adev = drm_to_adev(crtc->dev);

	appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_SIGNAL_READINESS);

	if (adev->mac_type == MAC_TYPE_T1)
		appletbdrm_t1_set_display_state(adev, T1_HID_DISPLAYSTATE_ON);
}

static void appletbdrm_crtc_helper_atomic_disable(struct drm_crtc *crtc,
						  struct drm_atomic_state *state)
{
	struct appletbdrm_device *adev = drm_to_adev(crtc->dev);

	appletbdrm_send_simple_cmd(adev, APPLETBDRM_MSG_CLEAR_DISPLAY);
}

static const struct drm_crtc_helper_funcs appletbdrm_crtc_helper_funcs = {
	.mode_valid	= appletbdrm_crtc_helper_mode_valid,
	.atomic_enable	= appletbdrm_crtc_helper_atomic_enable,
	.atomic_disable	= appletbdrm_crtc_helper_atomic_disable,
};

static const struct drm_crtc_funcs appletbdrm_crtc_funcs = {
	.reset			= drm_atomic_helper_crtc_reset,
	.destroy		= drm_crtc_cleanup,
	.set_config		= drm_atomic_helper_set_config,
	.page_flip		= drm_atomic_helper_page_flip,
	.atomic_duplicate_state	= drm_atomic_helper_crtc_duplicate_state,
	.atomic_destroy_state	= drm_atomic_helper_crtc_destroy_state,
};

static const struct drm_encoder_funcs appletbdrm_encoder_funcs = {
	.destroy = drm_encoder_cleanup,
};

DEFINE_DRM_GEM_FOPS(appletbdrm_drm_fops);

static const struct drm_driver appletbdrm_drm_driver = {
	DRM_GEM_SHMEM_DRIVER_OPS,
	.name			= "appletbdrm",
	.desc			= "Apple Touch Bar DRM Driver",
	.major			= 1,
	.minor			= 2,
	.driver_features	= DRIVER_MODESET | DRIVER_GEM | DRIVER_ATOMIC,
	.fops			= &appletbdrm_drm_fops,
};

static int appletbdrm_find_bulk_endpoints(struct usb_interface *intf,
					  struct usb_endpoint_descriptor **in_ep,
					  struct usb_endpoint_descriptor **out_ep)
{
	struct usb_device *udev = interface_to_usbdev(intf);
	struct usb_host_config *cfg = udev->actconfig;
	struct usb_interface *candidate;
	int ret;
	int i;

	ret = usb_find_common_endpoints(intf->cur_altsetting, in_ep, out_ep, NULL, NULL);
	if (!ret)
		return 0;

	if (!cfg)
		return ret;

	dev_warn(&intf->dev,
		 "No bulk endpoints on ifnum=%u alt=%u (ret=%d), scanning active config interfaces\n",
		 intf->cur_altsetting->desc.bInterfaceNumber,
		 intf->cur_altsetting->desc.bAlternateSetting,
		 ret);

	for (i = 0; i < cfg->desc.bNumInterfaces; i++) {
		candidate = cfg->interface[i];
		if (!candidate || !candidate->cur_altsetting)
			continue;

		ret = usb_find_common_endpoints(candidate->cur_altsetting, in_ep, out_ep,
						NULL, NULL);
		if (ret)
			continue;

		dev_info(&intf->dev,
			 "Using bulk endpoints from ifnum=%u alt=%u class=0x%02x in=0x%02x out=0x%02x\n",
			 candidate->cur_altsetting->desc.bInterfaceNumber,
			 candidate->cur_altsetting->desc.bAlternateSetting,
			 candidate->cur_altsetting->desc.bInterfaceClass,
			 (*in_ep)->bEndpointAddress,
			 (*out_ep)->bEndpointAddress);
		return 0;
	}

	return -ENODEV;
}

static int appletbdrm_setup_mode_config(struct appletbdrm_device *adev)
{
	struct drm_connector *connector = &adev->connector;
	struct drm_plane *primary_plane = &adev->primary_plane;
	struct drm_crtc *crtc = &adev->crtc;
	struct drm_encoder *encoder = &adev->encoder;
	struct drm_device *drm = &adev->drm;
	int ret;

	ret = drmm_mode_config_init(drm);
	if (ret)
		return ret;

	ret = drm_universal_plane_init(drm, primary_plane, 0,
				       &appletbdrm_primary_plane_funcs,
				       appletbdrm_primary_plane_formats,
				       ARRAY_SIZE(appletbdrm_primary_plane_formats),
				       NULL, DRM_PLANE_TYPE_PRIMARY, NULL);
	if (ret)
		return ret;

	drm_plane_helper_add(primary_plane, &appletbdrm_primary_plane_helper_funcs);
	drm_plane_enable_fb_damage_clips(primary_plane);

	ret = drm_crtc_init_with_planes(drm, crtc, primary_plane, NULL,
					&appletbdrm_crtc_funcs, NULL);
	if (ret)
		return ret;

	drm_crtc_helper_add(crtc, &appletbdrm_crtc_helper_funcs);

	ret = drm_encoder_init(drm, encoder, &appletbdrm_encoder_funcs,
			       DRM_MODE_ENCODER_DAC, NULL);
	if (ret)
		return ret;

	encoder->possible_crtcs = drm_crtc_mask(crtc);

	drm->mode_config.max_width = max(adev->height, DRM_SHADOW_PLANE_MAX_WIDTH);
	drm->mode_config.max_height = max(adev->width, DRM_SHADOW_PLANE_MAX_HEIGHT);
	drm->mode_config.preferred_depth = APPLETBDRM_BITS_PER_PIXEL;
	drm->mode_config.funcs = &appletbdrm_mode_config_funcs;
	/*
	 * Use portrait orientation: hdisplay = short side (adev->height for T1 = 60),
	 * vdisplay = long side (adev->width for T1 = 2170).
	 * tiny-dfr's draw() rotates 90° and expects (height=short, width=long),
	 * which it gets from mode.size() = (hdisplay=60, vdisplay=2170) via the
	 * variable-swap `let (height, width) = mode.size()`.
	 */
	adev->mode = (struct drm_display_mode) {
		DRM_MODE_INIT(60, adev->height, adev->width,
			      adev->height_mm, adev->width_mm)
	};

	ret = drm_connector_init(drm, connector, &appletbdrm_connector_funcs,
				 DRM_MODE_CONNECTOR_USB);
	if (ret)
		return ret;

	/*
	 * Force the initial status to connected so userspace (tiny-dfr) sees it
	 * immediately without needing to trigger a fill_modes/detect cycle.
	 * drm_connector_init() leaves status as connector_status_unknown which
	 * causes DRM_IOCTL_MODE_GETCONNECTOR to report "unknown" until the first
	 * full probe, and some userspace doesn't wait for that.
	 */
	connector->status = connector_status_connected;

	drm_connector_helper_add(connector, &appletbdrm_connector_helper_funcs);

	/*
	 * Pre-populate connector->modes with our fixed mode.  The kernel's
	 * drm_mode_getconnector ioctl only calls fill_modes() when the caller
	 * is the current DRM master, but tiny-dfr probes connectors *before*
	 * acquiring master (it acquires master after finding the right card).
	 * Without pre-population the modes list is empty and tiny-dfr rejects
	 * the connector even though the device and connector are fully ready.
	 *
	 * drm_helper_probe_single_connector_modes (fill_modes) will clear this
	 * and re-populate it properly the first time it is called by a master.
	 */
	{
		struct drm_display_mode *pre_mode =
			drm_mode_duplicate(drm, &adev->mode);
		if (pre_mode) {
			pre_mode->type = DRM_MODE_TYPE_DRIVER |
					 DRM_MODE_TYPE_PREFERRED;
			pre_mode->status = MODE_OK;
			list_add_tail(&pre_mode->head, &connector->modes);
		}
	}

	connector->display_info.width_mm = adev->height_mm;
	connector->display_info.height_mm = adev->width_mm;

	ret = drm_connector_set_panel_orientation(connector,
				DRM_MODE_PANEL_ORIENTATION_RIGHT_UP);
	if (ret)
		return ret;

	connector->display_info.non_desktop = true;
	ret = drm_object_property_set_value(&connector->base,
					    drm->mode_config.non_desktop_property,
					    true);
	if (ret)
		return ret;

	ret = drm_connector_attach_encoder(connector, encoder);
	if (ret)
		return ret;

	drm_mode_config_reset(drm);
	return 0;
}

static int appletbdrm_probe(struct usb_interface *intf,
			    const struct usb_device_id *id)
{
	struct appletbdrm_device *adev;
	struct usb_endpoint_descriptor *in_ep, *out_ep;
	int ret;
	ret = appletbdrm_find_bulk_endpoints(intf, &in_ep, &out_ep);
	if (ret) {
		dev_err(&intf->dev, "Bulk endpoints not found: %d\n", ret);
		return ret;
	}

	adev = devm_drm_dev_alloc(&intf->dev, &appletbdrm_drm_driver,
				  struct appletbdrm_device, drm);
	if (IS_ERR(adev))
		return PTR_ERR(adev);

	adev->dmadev = &intf->dev;
	adev->udev = interface_to_usbdev(intf);
	adev->interface = intf;
	adev->in_ep = in_ep->bEndpointAddress;
	adev->out_ep = out_ep->bEndpointAddress;

	if (id->idProduct == IBRIDGE_PID) {
		adev->mac_type = MAC_TYPE_T1;
	} else if (id->idProduct == IBRIDGE_PID_T2) {
		adev->mac_type = MAC_TYPE_T2;
	} else {
		return -ENODEV;
	}

	dev_info(adev->dmadev, "Touch Bar %s EP in=0x%02x out=0x%02x\n",
		 adev->mac_type == MAC_TYPE_T1 ? "T1" : "T2",
		 adev->in_ep, adev->out_ep);

	/*
	 * Always init the work struct so cancel_work_sync() in disconnect is
	 * safe even when probe fails before INIT_WORK would otherwise be called.
	 */
	INIT_WORK(&adev->init_work, appletbdrm_init_work_fn);
	init_completion(&adev->ginf_completion);

	usb_set_intfdata(intf, adev);

	if (adev->mac_type == MAC_TYPE_T1) {
		ret = appletbdrm_probe_t1_sync(adev);
		if (ret)
			goto err_release_hid;
	} else {
		/*
		 * T2: Synchronous initialization like upstream.
		 * No async URB needed - avoids USB bandwidth contention
		 * with HID multitouch driver.
		 */
		ret = appletbdrm_probe_t2_sync(adev);
		if (ret)
			goto err_release_hid;
	}

	return 0;

err_release_hid:
	appletbdrm_release_hid_interface(adev);
	return ret;
}

static void appletbdrm_disconnect(struct usb_interface *intf)
{
	struct appletbdrm_device *adev = usb_get_intfdata(intf);
	int ifnum;

	if (!adev)
		return;

	ifnum = intf->cur_altsetting->desc.bInterfaceNumber;
	if (ifnum == T1_HID_INTERFACE_NUM)
		return;

	appletbdrm_release_hid_interface(adev);

	/*
	 * drm_dev_unplug and drm_atomic_helper_shutdown require an initialized
	 * DRM mode config (specifically mode_config.mutex / ww_mutex).
	 * appletbdrm_setup_mode_config is only called when probe succeeds, so
	 * guard these calls behind drm_registered to avoid a kernel oops when
	 * probe failed (e.g. GINF timeout) but disconnect is still invoked
	 * because usb_set_intfdata was already set.
	 */
	if (adev->drm_registered) {
		drm_dev_unplug(&adev->drm);
		drm_atomic_helper_shutdown(&adev->drm);
	}

	cancel_work_sync(&adev->init_work);

	if (adev->async_in_urb) {
		usb_kill_urb(adev->async_in_urb);
		kfree(adev->async_in_buffer);
		usb_free_urb(adev->async_in_urb);
	}
}

static void appletbdrm_shutdown(struct usb_interface *intf)
{
	struct appletbdrm_device *adev = usb_get_intfdata(intf);

	if (adev && adev->drm_registered)
		drm_atomic_helper_shutdown(&adev->drm);
}

static const struct usb_device_id appletbdrm_usb_id_table[] = {
	{ USB_DEVICE_INTERFACE_NUMBER(APPLE_VID, IBRIDGE_PID,
				      IBRIDGE_INTERFACE_NUM) },
	{ USB_DEVICE_INTERFACE_CLASS(APPLE_VID, IBRIDGE_PID_T2, 0x10) },
	{}
};
MODULE_DEVICE_TABLE(usb, appletbdrm_usb_id_table);

static struct usb_driver appletbdrm_usb_driver = {
	.name		= "appletbdrm",
	.probe		= appletbdrm_probe,
	.disconnect	= appletbdrm_disconnect,
	.shutdown	= appletbdrm_shutdown,
	.id_table	= appletbdrm_usb_id_table,
};

module_usb_driver(appletbdrm_usb_driver);

MODULE_AUTHOR("Kerem Karabay <kekrby@gmail.com>");
MODULE_AUTHOR("sunplex07");
MODULE_DESCRIPTION("Apple Touch Bar DRM Driver");
MODULE_LICENSE("GPL");
