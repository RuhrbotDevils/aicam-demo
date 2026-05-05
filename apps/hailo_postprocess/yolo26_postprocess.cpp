// Implements Hailo post-processing routines for model inference output.
// Author: Thomas Klute

/**
 * YOLO26 NMS-free post-processing for hailofilter GStreamer element.
 *
 * Handles any correctly-compiled NMS-free YOLO26 HEF - regardless of
 * class count, input resolution, training dataset, or DFC-assigned
 * network-name prefix. The previous version of this file hardcoded
 * tensor names, input size, and COCO class labels and silently failed
 * for any other model.
 *
 * --------------------------------------------------------------------
 * Contract (see docs/hailo/yolo26_hailo.md and the DFC hint in
 * experiments/04_pt_vs_hef/hailo_sdk.client.log):
 * --------------------------------------------------------------------
 *  - HEF exported from Ultralytics with end2end=True (one-to-one head)
 *  - .alls has normalization(...) but NO nms_postprocess(...)
 *  - HEF emits exactly 6 NHWC output tensors, three scales x
 *    { box (C=4, LTRB distances), class (C=nc, raw class logits) }.
 *    Tensors may be UINT8 or UINT16 depending on how the DFC
 *    compiled the heads - decode_scale() dispatches on
 *    HailoTensorFormatType at runtime. (Blindly calling
 *    get_uint16() on a UINT8 tensor reads 2× past the buffer
 *    and SEGVs.)
 *  - Network-name prefix on the tensors is irrelevant here; tensors
 *    are discovered by channel count and sorted by grid size.
 *  - Strides are assumed to be 8 / 16 / 32 (YOLO convention that the
 *    DFC follows).
 *  - Input W/H is derived from the largest grid x 8.
 *
 * Label strings come from the environment variable
 *   AICAM_YOLO26_POST_LABELS   (comma-separated, e.g. "ball,robot,human")
 * with a built-in COCO fallback when unset and nc == 80, and
 * "class_0".."class_{N-1}" for other class counts.
 *
 * Confidence threshold comes from
 *   AICAM_YOLO26_POST_CONF_THRESHOLD   (float, default 0.25)
 *
 * --------------------------------------------------------------------
 * GStreamer usage
 * --------------------------------------------------------------------
 *   hailonet hef-path=<model>.hef !
 *   hailofilter so-path=libyolo26_post.so function-name=yolo26 qos=false !
 *   hailooverlay
 *
 * Build on Pi:
 *   make -C apps/hailo_postprocess
 *
 * --------------------------------------------------------------------
 * Failure modes - loud, never silent
 * --------------------------------------------------------------------
 *  - Fewer than 6 tensors, or not 3 pairs with (C=4) + (C=nc) -> GST_WARNING
 *    "yolo26_post: unexpected tensor layout - expected 6 tensors in
 *    3 pairs of box(C=4)+class(C=nc); got N tensors with shapes ..."
 *    Returns with zero detections. HEF was likely compiled with the
 *    wrong end_node_names. See docs/hailo/yolo26_recompile_guide.md.
 */

#include "hailo_common.hpp"
#include "hailo_objects.hpp"
#include "hailo_tensors.hpp"

#include <gst/gst.h>

#include <algorithm>
#include <array>
#include <cmath>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <sstream>
#include <string>
#include <vector>

GST_DEBUG_CATEGORY_STATIC(yolo26_post_debug);
#define GST_CAT_DEFAULT yolo26_post_debug

namespace {

constexpr int BOX_CHANNELS = 4;
constexpr std::array<int, 3> STANDARD_STRIDES = {8, 16, 32};

const char *const COCO_NAMES[] = {
    "person",        "bicycle",      "car",
    "motorcycle",    "airplane",     "bus",
    "train",         "truck",        "boat",
    "traffic light", "fire hydrant", "stop sign",
    "parking meter", "bench",        "bird",
    "cat",           "dog",          "horse",
    "sheep",         "cow",          "elephant",
    "bear",          "zebra",        "giraffe",
    "backpack",      "umbrella",     "handbag",
    "tie",           "suitcase",     "frisbee",
    "skis",          "snowboard",    "sports ball",
    "kite",          "baseball bat", "baseball glove",
    "skateboard",    "surfboard",    "tennis racket",
    "bottle",        "wine glass",   "cup",
    "fork",          "knife",        "spoon",
    "bowl",          "banana",       "apple",
    "sandwich",      "orange",       "broccoli",
    "carrot",        "hot dog",      "pizza",
    "donut",         "cake",         "chair",
    "couch",         "potted plant", "bed",
    "dining table",  "toilet",       "tv",
    "laptop",        "mouse",        "remote",
    "keyboard",      "cell phone",   "microwave",
    "oven",          "toaster",      "sink",
    "refrigerator",  "book",         "clock",
    "vase",          "scissors",     "teddy bear",
    "hair drier",    "toothbrush",
};
constexpr int COCO_COUNT = sizeof(COCO_NAMES) / sizeof(COCO_NAMES[0]);

inline float sigmoid(float x) { return 1.0f / (1.0f + std::exp(-x)); }

std::vector<std::string> split_csv(const char *csv) {
    std::vector<std::string> out;
    if (csv == nullptr || csv[0] == '\0')
        return out;
    std::string cur;
    for (const char *p = csv; *p; ++p) {
        if (*p == ',') {
            out.push_back(cur);
            cur.clear();
        } else {
            cur.push_back(*p);
        }
    }
    if (!cur.empty())
        out.push_back(cur);
    return out;
}

// Labels are looked up on every frame so that a pipeline rebuild on
// model switch picks up the new AICAM_YOLO26_POST_LABELS value
// without requiring a media-service process restart.
// For max 80 classes this is low-microsecond overhead at 3 FPS.
std::vector<std::string> labels_for(int num_classes) {
    auto env_labels = split_csv(std::getenv("AICAM_YOLO26_POST_LABELS"));
    if (!env_labels.empty())
        return env_labels;
    if (num_classes == COCO_COUNT) {
        std::vector<std::string> coco;
        coco.reserve(COCO_COUNT);
        for (int i = 0; i < COCO_COUNT; ++i)
            coco.emplace_back(COCO_NAMES[i]);
        return coco;
    }
    std::vector<std::string> numeric;
    numeric.reserve(num_classes);
    for (int i = 0; i < num_classes; ++i)
        numeric.emplace_back("class_" + std::to_string(i));
    return numeric;
}

float conf_threshold() {
    const char *env = std::getenv("AICAM_YOLO26_POST_CONF_THRESHOLD");
    if (env == nullptr || env[0] == '\0')
        return 0.25f;
    try {
        float v = std::stof(env);
        if (v > 0.0f && v < 1.0f)
            return v;
    } catch (...) {
        // fall through
    }
    return 0.25f;
}

struct ScalePair {
    HailoTensorPtr box;    // (H, W, 4)
    HailoTensorPtr cls;    // (H, W, nc)
    int grid_h;
    int grid_w;
    int stride;
};

// Discover 3 pairs of (box, class) tensors by channel count.
// Returns empty vector if the layout doesn't match the contract.
std::vector<ScalePair> discover_scales(const std::vector<HailoTensorPtr> &tensors) {
    std::vector<HailoTensorPtr> box_tensors, cls_tensors;
    int cls_channels = -1;

    for (auto &t : tensors) {
        if (!t)
            continue;
        int ch = static_cast<int>(t->features());
        if (ch == BOX_CHANNELS) {
            box_tensors.push_back(t);
        } else if (ch > 0) {
            if (cls_channels == -1) {
                cls_channels = ch;
                cls_tensors.push_back(t);
            } else if (ch == cls_channels) {
                cls_tensors.push_back(t);
            }
        }
    }

    if (box_tensors.size() != 3 || cls_tensors.size() != 3) {
        return {};
    }

    // Sort each list by grid width, descending (stride 8 first).
    auto by_width_desc = [](const HailoTensorPtr &a, const HailoTensorPtr &b) {
        return a->width() > b->width();
    };
    std::sort(box_tensors.begin(), box_tensors.end(), by_width_desc);
    std::sort(cls_tensors.begin(), cls_tensors.end(), by_width_desc);

    // Require grids to match across box / class at each scale, and to
    // follow the 1:2:4 ratio implied by strides 8/16/32.
    std::vector<ScalePair> pairs;
    for (size_t i = 0; i < 3; ++i) {
        auto &bx = box_tensors[i];
        auto &cl = cls_tensors[i];
        if (bx->width() != cl->width() || bx->height() != cl->height())
            return {};
        pairs.push_back({bx, cl, static_cast<int>(bx->height()),
                         static_cast<int>(bx->width()), STANDARD_STRIDES[i]});
    }

    // Sanity: grid widths must be in 4:2:1 ratio. Otherwise the DFC
    // was compiled with non-standard strides and our stride table is
    // wrong - better to fail loudly.
    if (pairs[0].grid_w != pairs[1].grid_w * 2 ||
        pairs[1].grid_w != pairs[2].grid_w * 2) {
        return {};
    }

    return pairs;
}

std::string shape_str(const std::vector<HailoTensorPtr> &tensors) {
    std::ostringstream oss;
    for (size_t i = 0; i < tensors.size(); ++i) {
        if (i > 0)
            oss << ", ";
        auto &t = tensors[i];
        if (!t) {
            oss << "(null)";
            continue;
        }
        oss << t->name() << " (" << t->height() << "x" << t->width()
            << "x" << t->features() << ")";
    }
    return oss.str();
}

// Dequantize a cell of a HailoTensor regardless of whether it was compiled
// as UINT8 or UINT16. get_uint16 reads two bytes per element - calling it
// on a UINT8 tensor reads 2× past the actual buffer and eventually SEGVs.
inline float dequant_cell(const HailoTensorPtr &t, int row, int col, int channel,
                          bool is_uint16) {
    return is_uint16 ? t->fix_scale(t->get_uint16(row, col, channel))
                     : t->fix_scale(t->get(row, col, channel));
}

void decode_scale(HailoROIPtr roi, const ScalePair &p, int num_classes,
                  int input_w, int input_h, const std::vector<std::string> &labels,
                  float conf_th) {
    const float logit_th = -std::log(1.0f / conf_th - 1.0f);
    const int grid_h = p.grid_h;
    const int grid_w = p.grid_w;
    const int stride = p.stride;
    const float inv_w = 1.0f / static_cast<float>(input_w);
    const float inv_h = 1.0f / static_cast<float>(input_h);
    const bool box_u16 = p.box->format().type == HailoTensorFormatType::HAILO_FORMAT_TYPE_UINT16;
    const bool cls_u16 = p.cls->format().type == HailoTensorFormatType::HAILO_FORMAT_TYPE_UINT16;

    for (int row = 0; row < grid_h; ++row) {
        for (int col = 0; col < grid_w; ++col) {
            // Argmax class logit for this cell.
            float max_logit = -1e9f;
            int best_cls = 0;
            for (int c = 0; c < num_classes; ++c) {
                float logit = dequant_cell(p.cls, row, col, c, cls_u16);
                if (logit > max_logit) {
                    max_logit = logit;
                    best_cls = c;
                }
            }
            if (max_logit <= logit_th)
                continue;

            float confidence = sigmoid(max_logit);

            float left = dequant_cell(p.box, row, col, 0, box_u16);
            float top = dequant_cell(p.box, row, col, 1, box_u16);
            float right = dequant_cell(p.box, row, col, 2, box_u16);
            float bottom = dequant_cell(p.box, row, col, 3, box_u16);

            float x1 = (static_cast<float>(col) + 0.5f - left) * static_cast<float>(stride);
            float y1 = (static_cast<float>(row) + 0.5f - top) * static_cast<float>(stride);
            float x2 = (static_cast<float>(col) + 0.5f + right) * static_cast<float>(stride);
            float y2 = (static_cast<float>(row) + 0.5f + bottom) * static_cast<float>(stride);

            x1 = std::max(0.0f, std::min(x1, static_cast<float>(input_w)));
            y1 = std::max(0.0f, std::min(y1, static_cast<float>(input_h)));
            x2 = std::max(0.0f, std::min(x2, static_cast<float>(input_w)));
            y2 = std::max(0.0f, std::min(y2, static_cast<float>(input_h)));

            float w = x2 - x1;
            float h = y2 - y1;
            if (w < 1.0f || h < 1.0f)
                continue;

            HailoBBox bbox(x1 * inv_w, y1 * inv_h, w * inv_w, h * inv_h);
            const std::string &label = (best_cls >= 0 && best_cls < static_cast<int>(labels.size()))
                                           ? labels[best_cls]
                                           : std::string("unknown");
            hailo_common::add_detection(roi, bbox, label, confidence, best_cls);
        }
    }
}

// Warn once per call-site per process, using a static flag. Keeps the
// log readable when the same bad HEF runs at 30 FPS.
void warn_once(const char *key, const std::string &msg) {
    static std::mutex mu;
    static std::vector<std::string> seen;
    std::lock_guard<std::mutex> lock(mu);
    for (auto &s : seen) {
        if (s == key)
            return;
    }
    seen.emplace_back(key);
    GST_WARNING("yolo26_post: %s", msg.c_str());
    g_warning("yolo26_post: %s", msg.c_str());
}

}  // namespace

extern "C" void yolo26(HailoROIPtr roi) {
    static std::once_flag init_cat;
    std::call_once(init_cat, []() {
        GST_DEBUG_CATEGORY_INIT(yolo26_post_debug, "yolo26_post", 0,
                                "YOLO26 NMS-free postprocess");
    });

    auto tensors = roi->get_tensors();
    auto pairs = discover_scales(tensors);
    if (pairs.empty()) {
        std::ostringstream oss;
        oss << "unexpected tensor layout - expected 6 tensors in 3 pairs "
               "of box(C=4)+class(C=nc); got "
            << tensors.size() << " tensors: " << shape_str(tensors)
            << ". HEF likely compiled with wrong end_node_names - see "
               "docs/hailo/yolo26_recompile_guide.md";
        warn_once("layout", oss.str());
        return;
    }

    int num_classes = static_cast<int>(pairs[0].cls->features());
    int input_w = pairs[0].grid_w * STANDARD_STRIDES[0];
    int input_h = pairs[0].grid_h * STANDARD_STRIDES[0];
    const auto &labels = labels_for(num_classes);
    float conf_th = conf_threshold();

    if (static_cast<int>(labels.size()) < num_classes) {
        std::ostringstream oss;
        oss << "label table has " << labels.size() << " entries but model "
            << "has " << num_classes << " classes - out-of-range class IDs "
            << "will be labelled \"unknown\". Set AICAM_YOLO26_POST_LABELS "
            << "to a CSV with " << num_classes << " entries.";
        warn_once("labels_short", oss.str());
    }

    for (const auto &p : pairs) {
        decode_scale(roi, p, num_classes, input_w, input_h, labels, conf_th);
    }
}

// Generic filter entry point (alternative name TAPPAS sometimes
// dispatches to by default).
extern "C" void filter(HailoROIPtr roi) { yolo26(roi); }
