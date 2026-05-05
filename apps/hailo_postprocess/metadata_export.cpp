// Implements Hailo post-processing routines for model inference output.
// Author: Thomas Klute

/**
 * Metadata export post-processing for hailofilter.
 *
 * Read-only hailofilter that walks the HailoROI tree produced by the
 * upstream detection / cascade stages and publishes the structured
 * contents on the ZMQ bus as:
 *
 *   - ``ai.object_detections`` → ObjectDetectionsMessage (one per frame)
 *   - ``ai.robot_attributes``  → RobotAttributesMessage  (one per frame,
 *                                may carry zero entries on frames with
 *                                only ball detections - the
 *                                BundleCollector uses the empty message
 *                                as a completeness signal)
 *
 * The function does **not** mutate the HailoROI tree, so ``hailooverlay``
 * downstream still renders boxes exactly as before. Placement in the
 * pipeline: after ``hailoaggregator`` (cascade) or after the detector
 * ``hailofilter`` (single-stage), before ``hailooverlay``.
 *
 * Messages follow the schemas in ``apps/schemas/object_detections.py``
 * and ``apps/schemas/robot_attributes.py``. JSON is hand-written (no
 * nlohmann dependency) to keep the build minimal. All strings are
 * escaped; numeric values are safe by construction.
 *
 * Coordinate system: ``HailoBBox`` values are normalized ``[0, 1]``
 * relative to the model input size (640 × 640 in our AI branch). The
 * meta_export filter multiplies them by the original camera frame
 * dimensions read from environment variables ``AICAM_META_EXPORT_WIDTH``
 * and ``AICAM_META_EXPORT_HEIGHT`` at initialization time. The
 * resulting bbox values are in image pixels, matching the
 * ``coordinate_system = "image_px"`` contract of ``ObjectDetectionsMessage``.
 *
 * Class label mapping: the upstream detector uses its own label space
 * (COCO names, YOLO26-robots names, whatever the .hef says). This
 * filter maps them to the strict ``DetectionClass`` enum
 * (``robot``/``human``/``ball``). Detections with no valid mapping
 * are skipped and NOT published - the pydantic schema on the consumer
 * would reject them anyway.
 *
 * Build on Pi:
 *   make -C apps/hailo_postprocess
 *
 * Wiring in pipeline.rs:
 *   let meta = try_create_element("hailofilter", "ai_meta_export")?;
 *   meta.set_property_from_str("so-path",
 *       "/opt/robocup-ai-camera/apps/hailo_postprocess/libmetadata_export.so");
 *   meta.set_property_from_str("function-name", "export_metadata");
 *   meta.set_property("qos", false);
 */

#include "hailo_common.hpp"
#include "hailo_objects.hpp"

#include <unistd.h>
#include <zmq.h>

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <mutex>
#include <sstream>
#include <string>
#include <vector>

// ---------------------------------------------------------------------------
// Topics + endpoints
// ---------------------------------------------------------------------------

static constexpr const char *DETECTIONS_TOPIC = "ai.object_detections";
static constexpr const char *ATTRIBUTES_TOPIC = "ai.robot_attributes";
static constexpr const char *BROKER_XSUB_ENDPOINT = "tcp://127.0.0.1:5559";

// Fallback frame dimensions if AICAM_META_EXPORT_WIDTH / HEIGHT are
// unset. Matches the default media service camera resolution; if the
// media service runs at a different resolution it MUST set the env
// vars before starting the pipeline.
static constexpr int DEFAULT_FRAME_WIDTH = 1920;
static constexpr int DEFAULT_FRAME_HEIGHT = 1080;

// ---------------------------------------------------------------------------
// Global state (ZMQ singleton + per-process session)
// ---------------------------------------------------------------------------

namespace {

std::mutex g_mutex;
bool g_initialized = false;
void *g_ctx = nullptr;
void *g_socket = nullptr;
std::string g_session_id;
std::atomic<uint64_t> g_frame_counter{0};
int g_frame_width = DEFAULT_FRAME_WIDTH;
int g_frame_height = DEFAULT_FRAME_HEIGHT;

// Optional index→name label maps for the cascade
// classifiers. Populated from env vars set by pipeline.rs at
// build time. When non-empty, walk_tree maps the raw
// HailoClassification label (which may be an integer index like
// "0", "1", ...) to the human-readable name from the map. When
// empty, the raw label from the postprocess .so is used as-is.
std::vector<std::string> g_cls1_labels;  // robot-type classifier
std::vector<std::string> g_cls2_labels;  // jersey-colour classifier

// Detector class-ID remapping. When active, g_det_class_map[class_id]
// gives the pipeline label ("ball", "robot", "human"). Empty string
// means "drop this class". Built once from env var at init.
constexpr size_t DET_CLASS_MAP_SIZE = 256;
std::string g_det_class_map[DET_CLASS_MAP_SIZE];
bool g_det_class_map_active = false;

/// Split a comma-separated string into a vector of strings.
std::vector<std::string> split_csv(const char *csv) {
    std::vector<std::string> out;
    if (!csv || csv[0] == '\0') return out;
    std::string s(csv);
    size_t pos = 0;
    while (pos < s.size()) {
        size_t comma = s.find(',', pos);
        if (comma == std::string::npos) comma = s.size();
        out.push_back(s.substr(pos, comma - pos));
        pos = comma + 1;
    }
    return out;
}

/// Look up a label from a label map. If the raw label is a valid
/// integer index into the map, return the mapped name; otherwise
/// return the raw label unchanged.
std::string map_label(const std::string &raw,
                      const std::vector<std::string> &label_map) {
    if (label_map.empty()) return raw;
    // Try to parse the raw label as an integer index.
    char *end = nullptr;
    long idx = std::strtol(raw.c_str(), &end, 10);
    if (end != raw.c_str() && *end == '\0' &&
        idx >= 0 && static_cast<size_t>(idx) < label_map.size()) {
        return label_map[static_cast<size_t>(idx)];
    }
    // Not a plain integer - return raw label unchanged.
    return raw;
}

// -------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------

std::string iso_timestamp_now() {
    using namespace std::chrono;
    auto now = system_clock::now();
    auto secs = time_point_cast<seconds>(now);
    auto us = duration_cast<microseconds>(now - secs).count();
    std::time_t t = system_clock::to_time_t(secs);
    std::tm tm_utc;
    gmtime_r(&t, &tm_utc);
    char buf[40];
    std::strftime(buf, sizeof(buf), "%Y-%m-%dT%H:%M:%S", &tm_utc);
    char out[64];
    std::snprintf(out, sizeof(out), "%s.%06ld+00:00", buf, (long)us);
    return out;
}

std::string json_escape(const std::string &s) {
    std::string out;
    out.reserve(s.size() + 2);
    for (char c : s) {
        switch (c) {
            case '"':  out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\b': out += "\\b"; break;
            case '\f': out += "\\f"; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default:
                if (static_cast<unsigned char>(c) < 0x20) {
                    char buf[8];
                    std::snprintf(buf, sizeof(buf), "\\u%04x", c);
                    out += buf;
                } else {
                    out += c;
                }
        }
    }
    return out;
}

// Map a Hailo detection label to our strict DetectionClass enum.
// Returns "" (empty) if the label does not map to any known class -
// such detections are dropped (schema would reject them downstream).
//
// The matching is intentionally permissive so different models
// (YOLOv8 COCO, YOLO26m-robots, custom humanoid classifiers) all
// feed the same downstream enum without per-model config.
std::string map_class_label(const std::string &label) {
    // Lowercase copy for case-insensitive substring matching.
    std::string l = label;
    for (char &c : l) {
        if (c >= 'A' && c <= 'Z') c = c + ('a' - 'A');
    }
    if (l.find("ball") != std::string::npos) return "ball";
    if (l.find("robot") != std::string::npos) return "robot";
    if (l.find("nao") != std::string::npos) return "robot";
    if (l.find("humanoid") != std::string::npos) return "robot";
    // COCO "person" is the closest stand-in for a robot in development
    // scenarios where we test the pipeline with a human in frame. The
    // tracker treats it as a robot - fine for smoke testing.
    if (l == "person") return "robot";
    if (l == "human") return "human";
    return "";
}

void initialize_once() {
    if (g_initialized) return;

    g_ctx = zmq_ctx_new();
    if (!g_ctx) {
        std::fprintf(stderr, "meta_export: zmq_ctx_new failed\n");
        return;
    }
    g_socket = zmq_socket(g_ctx, ZMQ_PUB);
    if (!g_socket) {
        std::fprintf(stderr, "meta_export: zmq_socket failed\n");
        zmq_ctx_term(g_ctx);
        g_ctx = nullptr;
        return;
    }
    // Non-blocking send; if the broker is unreachable, messages drop
    // after the linger window rather than blocking the pipeline.
    int linger_ms = 100;
    zmq_setsockopt(g_socket, ZMQ_LINGER, &linger_ms, sizeof(linger_ms));

    if (zmq_connect(g_socket, BROKER_XSUB_ENDPOINT) != 0) {
        std::fprintf(stderr, "meta_export: zmq_connect(%s) failed: %s\n",
                     BROKER_XSUB_ENDPOINT, zmq_strerror(zmq_errno()));
        // Keep going - ZMQ reconnects automatically once the broker
        // comes up. Messages sent in the meantime are dropped.
    }

    // Session ID: "media-<epoch_ms>-<pid>". Stable for the life of
    // the process, unique per pipeline start. Embedded in every
    // message so downstream consumers can tell one session apart
    // from the next.
    using namespace std::chrono;
    int64_t ms = duration_cast<milliseconds>(
                     system_clock::now().time_since_epoch())
                     .count();
    char sid[64];
    std::snprintf(sid, sizeof(sid), "media-%lld-%d", (long long)ms, (int)getpid());
    g_session_id = sid;

    // Frame dimensions from env vars.
    const char *w = std::getenv("AICAM_META_EXPORT_WIDTH");
    const char *h = std::getenv("AICAM_META_EXPORT_HEIGHT");
    if (w) g_frame_width = std::atoi(w);
    if (h) g_frame_height = std::atoi(h);
    if (g_frame_width <= 0) g_frame_width = DEFAULT_FRAME_WIDTH;
    if (g_frame_height <= 0) g_frame_height = DEFAULT_FRAME_HEIGHT;

    // Classifier label maps from env vars.
    g_cls1_labels = split_csv(std::getenv("AICAM_META_EXPORT_CLS1_LABELS"));
    g_cls2_labels = split_csv(std::getenv("AICAM_META_EXPORT_CLS2_LABELS"));

    // Detector class-ID remapping. Format: "0:human,32:ball"
    const char *remap_csv = std::getenv("AICAM_META_EXPORT_DET_CLASS_MAP");
    if (remap_csv && remap_csv[0] != '\0') {
        auto entries = split_csv(remap_csv);
        for (const auto &entry : entries) {
            auto colon = entry.find(':');
            if (colon == std::string::npos) continue;
            int id = std::atoi(entry.substr(0, colon).c_str());
            if (id >= 0 && static_cast<size_t>(id) < DET_CLASS_MAP_SIZE) {
                g_det_class_map[id] = entry.substr(colon + 1);
            }
        }
        g_det_class_map_active = true;
        std::fprintf(stderr, "meta_export: det_class_map active (%zu entries)\n",
                     entries.size());
    }

    std::fprintf(stderr,
                 "meta_export: initialized session=%s frame=%dx%d "
                 "cls1_labels=%zu cls2_labels=%zu broker=%s\n",
                 g_session_id.c_str(), g_frame_width, g_frame_height,
                 g_cls1_labels.size(), g_cls2_labels.size(),
                 BROKER_XSUB_ENDPOINT);
    g_initialized = true;
}

void send_message(const char *topic, const std::string &payload) {
    if (!g_socket) return;
    // Two-part frame: [topic, payload], matching apps/bus/publisher.py.
    zmq_msg_t topic_msg;
    zmq_msg_init_size(&topic_msg, std::strlen(topic));
    std::memcpy(zmq_msg_data(&topic_msg), topic, std::strlen(topic));
    if (zmq_msg_send(&topic_msg, g_socket, ZMQ_SNDMORE | ZMQ_DONTWAIT) < 0) {
        zmq_msg_close(&topic_msg);
        return;
    }
    zmq_msg_t body_msg;
    zmq_msg_init_size(&body_msg, payload.size());
    std::memcpy(zmq_msg_data(&body_msg), payload.data(), payload.size());
    zmq_msg_send(&body_msg, g_socket, ZMQ_DONTWAIT);
}

struct DetectionView {
    std::string detection_id;
    std::string cls;           // mapped DetectionClass value
    float bbox_x_px;
    float bbox_y_px;
    float bbox_w_px;
    float bbox_h_px;
    float confidence;
    // Cascade classification sub-objects (may be absent).
    // When two classifiers are chained sequentially on
    // the crops path, the HailoROI tree carries two
    // HailoClassification objects per detection - one for robot
    // type and one for jersey colour.
    bool has_robot_type;
    std::string robot_type_label;
    float robot_type_confidence;
    bool has_jersey_color;
    std::string jersey_color_label;
    float jersey_color_confidence;
};

std::vector<DetectionView> walk_tree(HailoROIPtr roi, const std::string &frame_id) {
    std::vector<DetectionView> out;
    auto detections = hailo_common::get_hailo_detections(roi);
    out.reserve(detections.size());

    static std::mutex seen_labels_mutex;
    static std::vector<std::string> seen_labels;

    for (size_t i = 0; i < detections.size(); i++) {
        const auto &det = detections[i];
        const std::string &raw_label = det->get_label();
        {
            std::lock_guard<std::mutex> lk(seen_labels_mutex);
            bool already = false;
            for (const auto &l : seen_labels) {
                if (l == raw_label) { already = true; break; }
            }
            if (!already) {
                seen_labels.push_back(raw_label);
                std::fprintf(stderr,
                             "meta_export: first-seen label=%s mapped=%s\n",
                             raw_label.c_str(),
                             map_class_label(raw_label).c_str());
            }
        }
        std::string cls;
        if (g_det_class_map_active) {
            int cid = det->get_class_id();
            if (cid >= 0 && static_cast<size_t>(cid) < DET_CLASS_MAP_SIZE) {
                cls = g_det_class_map[cid];
            }
        } else {
            cls = map_class_label(raw_label);
        }
        if (cls.empty()) continue;

        DetectionView dv;
        char did[192];
        std::snprintf(did, sizeof(did), "%s-%zu", frame_id.c_str(), i);
        dv.detection_id = did;
        dv.cls = std::move(cls);
        dv.confidence = det->get_confidence();

        HailoBBox bb = det->get_bbox();
        // Bboxes are normalized [0, 1] relative to the model input.
        // Scale to configured frame dimensions.
        dv.bbox_x_px = bb.xmin() * static_cast<float>(g_frame_width);
        dv.bbox_y_px = bb.ymin() * static_cast<float>(g_frame_height);
        dv.bbox_w_px = bb.width() * static_cast<float>(g_frame_width);
        dv.bbox_h_px = bb.height() * static_cast<float>(g_frame_height);

        dv.has_robot_type = false;
        dv.has_jersey_color = false;
        // Cascade classifier output. When a
        // single classifier is chained, the HailoROI tree
        // has one HailoClassification. When two classifiers are
        // chained sequentially, the tree has two - the
        // first is from the robot-type classifier and the second is
        // from the jersey-colour classifier, matching the order they
        // appear in the GStreamer pipeline. We walk them by index:
        // index 0 = robot_type, index 1 = jersey_color.
        if (dv.cls == "robot") {
            auto classifications =
                hailo_common::get_hailo_classifications(det);
            if (classifications.size() >= 1) {
                const auto &rt = classifications[0];
                dv.has_robot_type = true;
                dv.robot_type_label =
                    map_label(rt->get_label(), g_cls1_labels);
                dv.robot_type_confidence = rt->get_confidence();
            }
            if (classifications.size() >= 2) {
                const auto &jc = classifications[1];
                dv.has_jersey_color = true;
                dv.jersey_color_label =
                    map_label(jc->get_label(), g_cls2_labels);
                dv.jersey_color_confidence = jc->get_confidence();
            }
        }
        out.push_back(std::move(dv));
    }
    return out;
}

std::string build_detections_message(const std::string &frame_id,
                                     const std::string &timestamp,
                                     const std::vector<DetectionView> &dets) {
    std::ostringstream j;
    j << "{";
    j << "\"schema_version\":\"1.0\",";
    j << "\"message_type\":\"object_detections\",";
    j << "\"message_id\":\"det-msg-" << json_escape(frame_id) << "\",";
    j << "\"session_id\":\"" << json_escape(g_session_id) << "\",";
    j << "\"source_module\":\"hailo_meta_export\",";
    j << "\"created_at\":\"" << timestamp << "\",";
    j << "\"frame_id\":\"" << json_escape(frame_id) << "\",";
    j << "\"source_timestamp\":\"" << timestamp << "\",";
    j << "\"detector_model\":{";
    j << "\"name\":\"hailo_meta_export\",";
    j << "\"version\":\"1.0\",";
    j << "\"runtime\":\"hailo\"";
    j << "},";
    j << "\"detections\":[";
    bool first = true;
    for (const auto &d : dets) {
        if (!first) j << ",";
        first = false;
        j << "{";
        j << "\"detection_id\":\"" << json_escape(d.detection_id) << "\",";
        j << "\"class\":\"" << d.cls << "\",";
        j << "\"bbox_xywh\":[" << d.bbox_x_px << "," << d.bbox_y_px << ","
          << d.bbox_w_px << "," << d.bbox_h_px << "],";
        j << "\"bbox_format\":\"xywh\",";
        j << "\"coordinate_system\":\"image_px\",";
        j << "\"confidence\":" << d.confidence;
        j << "}";
    }
    j << "]}";
    return j.str();
}

std::string build_attributes_message(const std::string &frame_id,
                                     const std::string &timestamp,
                                     const std::vector<DetectionView> &dets) {
    std::ostringstream j;
    j << "{";
    j << "\"schema_version\":\"1.0\",";
    j << "\"message_type\":\"robot_attributes\",";
    j << "\"message_id\":\"attr-msg-" << json_escape(frame_id) << "\",";
    j << "\"session_id\":\"" << json_escape(g_session_id) << "\",";
    j << "\"source_module\":\"hailo_meta_export\",";
    j << "\"created_at\":\"" << timestamp << "\",";
    j << "\"frame_id\":\"" << json_escape(frame_id) << "\",";
    j << "\"source_timestamp\":\"" << timestamp << "\",";
    j << "\"attributes\":[";
    bool first = true;
    for (const auto &d : dets) {
        if (!d.has_robot_type && !d.has_jersey_color) continue;
        if (!first) j << ",";
        first = false;
        j << "{";
        j << "\"detection_id\":\"" << json_escape(d.detection_id) << "\",";
        // robot_type (from cascade classifier 1, if present)
        if (d.has_robot_type) {
            j << "\"robot_type\":{";
            j << "\"label\":\"" << json_escape(d.robot_type_label) << "\",";
            j << "\"confidence\":" << d.robot_type_confidence;
            j << "},";
        } else {
            j << "\"robot_type\":null,";
        }
        j << "\"posture\":null,";
        // jersey_color (from cascade classifier 2, if present)
        if (d.has_jersey_color) {
            j << "\"jersey_color\":{";
            j << "\"color_space\":\"label\",";
            j << "\"value\":[0,0,0],";
            j << "\"label_hint\":\"" << json_escape(d.jersey_color_label) << "\",";
            j << "\"confidence\":" << d.jersey_color_confidence;
            j << "}";
        } else {
            j << "\"jersey_color\":null";
        }
        j << "}";
    }
    j << "]}";
    return j.str();
}

}  // namespace

// ---------------------------------------------------------------------------
// Public entry point - called by hailofilter on every buffer
// ---------------------------------------------------------------------------

extern "C" void export_metadata(HailoROIPtr roi) {
    std::lock_guard<std::mutex> lock(g_mutex);
    initialize_once();
    if (!g_socket) return;

    uint64_t frame_n = g_frame_counter.fetch_add(1);
    char fid[128];
    std::snprintf(fid, sizeof(fid), "%s-%lu", g_session_id.c_str(),
                  (unsigned long)frame_n);
    std::string frame_id = fid;

    std::string timestamp = iso_timestamp_now();

    auto dets = walk_tree(roi, frame_id);

    // ObjectDetectionsMessage - always sent, even on empty frames
    // (the BundleCollector treats it as the per-frame completeness anchor).
    std::string det_payload = build_detections_message(frame_id, timestamp, dets);
    send_message(DETECTIONS_TOPIC, det_payload);

    // RobotAttributesMessage - always sent, even with zero entries,
    // so the BundleCollector sees the enricher report per frame.
    std::string attr_payload = build_attributes_message(frame_id, timestamp, dets);
    send_message(ATTRIBUTES_TOPIC, attr_payload);
}

// Convenience alias for hailofilter's default `filter` entry-point name.
extern "C" void filter(HailoROIPtr roi) { export_metadata(roi); }
