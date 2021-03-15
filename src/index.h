#pragma once

#include "versioned_object.h"
#include "user_id.h"

#include <map>
#include <set>
#include <string>

namespace ouisync {

class BlockStore;

class Index {
private:
    template<class K, class V> using Map = std::map<K, V>;
    using ParentId = BlockId;
    using Count = uint32_t;
    using UserMap = Map<UserId, Count>;
    using ParentMap = Map<ParentId, UserMap>;
    using BlockMap = Map<BlockId, ParentMap>;

    const BlockId&  id(BlockMap ::const_iterator i) { return i->first; }
    const ParentId& id(ParentMap::const_iterator i) { return i->first; }
    const UserId&   id(UserMap  ::const_iterator i) { return i->first; }

          ParentMap& parents(BlockMap::iterator       i) { return i->second; }
    const ParentMap& parents(BlockMap::const_iterator i) { return i->second; }

          UserMap& users(ParentMap::iterator       i) { return i->second; }
    const UserMap& users(ParentMap::const_iterator i) { return i->second; }

    const Count& count(UserMap::const_iterator i) { return i->second; }
          Count& count(UserMap::iterator i) { return i->second; }

    struct Item;

public:
    Index() {}
    Index(const UserId&, VersionedObject);

    void set_commit(const UserId&, const VersionedObject&);
    void set_version_vector(const UserId&, const VersionVector&);

    void insert_block(const UserId&, const BlockId& id, const ParentId& parent_id, size_t cnt = 1);
    void remove_block(const UserId&, const BlockId& id, const ParentId& parent_id);

    void merge(const Index&, BlockStore&);

    Opt<VersionedObject> commit(const UserId&);
    const Map<UserId, VersionedObject>& commits() const { return _commits; }

    friend std::ostream& operator<<(std::ostream&, const Index&);

    const std::set<BlockId>& missing_blocks() const { return _missing_blocks; }

    bool someone_has(const BlockId&) const;
    bool block_is_missing(const BlockId&) const;

    // Return true if the block was previously marked as missing.
    bool mark_not_missing(const BlockId&);

    std::set<BlockId> roots() const;

    template<class Archive>
    void serialize(Archive& ar, unsigned) {
        ar & _blocks & _commits & _missing_blocks;
    }

    bool remote_is_newer(const VersionedObject& remote_commit, const UserId&) const;

    friend std::ostream& operator<<(std::ostream&, const Index&);

private:
    template<class F> void compare(const BlockMap&, F&&);

private:
    BlockMap _blocks;
    Map<UserId, VersionedObject> _commits;
    std::set<BlockId> _missing_blocks;
};

} // namespace
