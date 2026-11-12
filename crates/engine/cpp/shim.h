#pragma once

#include <cstdint>
#include <exception>
#include <memory>
#include <string>

#include "rust/cxx.h"
#include <libtorrent/session.hpp>
#include <libtorrent/torrent_handle.hpp>

namespace torrential {

std::string describe_exception(std::exception_ptr error);

}

// cxx calls this around every bridge function declared to return Result. The
// default only catches std::exception, and anything else escaping a noexcept
// frame would abort the process.
namespace rust {
namespace behavior {
template <typename Try, typename Fail>
static void trycatch(Try&& func, Fail&& fail) noexcept try {
  func();
} catch (...) {
  std::string const message = torrential::describe_exception(std::current_exception());
  fail(message.c_str());
}
}
}

namespace torrential {

struct SessionSettings;
struct AddParams;
struct AddResult;
struct ParsedTorrent;
struct FileRecord;
struct StatusRecord;
struct AlertRecord;
struct PriorityRecord;
struct PieceCounts;
struct MissingCounts;

class Session {
 public:
  explicit Session(SessionSettings const& settings);

  AddResult add_torrent(AddParams const& params);
  void pause(rust::Slice<const std::uint8_t> id);
  void resume(rust::Slice<const std::uint8_t> id);
  void set_file_priorities(rust::Slice<const std::uint8_t> id,
                           rust::Slice<const PriorityRecord> priorities);
  void force_recheck(rust::Slice<const std::uint8_t> id);
  void announce_to_dht(rust::Slice<const std::uint8_t> id);
  void save_resume_data(rust::Slice<const std::uint8_t> id);
  void save_resume_data_if_progressed(rust::Slice<const std::uint8_t> id);
  void remove(rust::Slice<const std::uint8_t> id);
  void connect_peer(rust::Slice<const std::uint8_t> id, rust::Str address, std::uint16_t port);
  void add_web_seed(rust::Slice<const std::uint8_t> id, rust::Str url);
  rust::Vec<std::uint8_t> metainfo(rust::Slice<const std::uint8_t> id) const;
  rust::Vec<std::uint8_t> info_dictionary(rust::Slice<const std::uint8_t> id) const;
  rust::Vec<FileRecord> files(rust::Slice<const std::uint8_t> id) const;
  StatusRecord status(rust::Slice<const std::uint8_t> id) const;
  rust::Vec<PieceCounts> piece_slices(rust::Slice<const std::uint8_t> id, std::uint32_t slices) const;
  MissingCounts missing_pieces(rust::Slice<const std::uint8_t> id) const;
  rust::Vec<PieceCounts> written_pieces(rust::Slice<const std::uint8_t> id, std::uint32_t file,
                                        std::uint64_t offset, std::uint64_t length, std::uint32_t runs) const;
  bool want_range(rust::Slice<const std::uint8_t> id, std::uint32_t file, std::uint64_t offset,
                  std::uint64_t length);
  void download_in_order(rust::Slice<const std::uint8_t> id);
  void post_session_stats();
  rust::Vec<std::uint8_t> save_state() const;
  std::uint16_t listen_port() const;
  rust::Vec<AlertRecord> wait_alerts(std::uint32_t timeout_ms);

 private:
  lt::torrent_handle find(rust::Slice<const std::uint8_t> id) const;
  lt::torrent_handle find_with_metadata(rust::Slice<const std::uint8_t> id) const;
  rust::Vec<std::uint8_t> torrent_file(rust::Slice<const std::uint8_t> id, bool piece_layers) const;

  struct StatsIndex {
    int sent_payload_bytes;
    int received_payload_bytes;
    int sent_bytes;
    int received_bytes;
    int dht_nodes;
    int peers_connected;
  };

  lt::session session_;
  StatsIndex stats_;
};

std::unique_ptr<Session> new_session(SessionSettings const& settings);
ParsedTorrent parse_magnet(rust::Str uri);
ParsedTorrent parse_metainfo(rust::Slice<const std::uint8_t> bytes);
ParsedTorrent parse_resume_data(rust::Slice<const std::uint8_t> bytes);
rust::Vec<std::uint8_t> create_torrent(rust::Str path, std::uint32_t piece_size, bool hybrid);
rust::String libtorrent_version();

}
