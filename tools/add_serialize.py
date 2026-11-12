import re, pathlib
root = pathlib.Path(r'D:\Dev\bilibili-slint\core\src')
targets = {
    'bilibili/feed.rs': ['ThumbStats', 'CdnKind', 'CdnMeasurement', 'CdnChoice', 'FeedVideo', 'VideoCard', 'CardPage', 'HotSearch', 'PlayInfo', 'QualityOption', 'VideoTrack', 'AudioTrack', 'QualityTier', 'VideoDetail', 'VideoPart', 'Season', 'SeasonSection', 'SeasonEpisode'],
    'bilibili/account.rs': ['VideoShot', 'NavUser', 'NavOutcome', 'VipCode', 'VipMyPrivilege', 'VipMyPage', 'VipPrivilegeItem', 'VipPrivilegeGroup', 'VipTask', 'VipTaskPage', 'Relation', 'CommentUser', 'Comment', 'CommentPage', 'CommentReplyPage', 'CommentLikeResult', 'UpOrder', 'UpInfo', 'SpaceVideo', 'SpaceVideoPage', 'FavFolder', 'FavFolderState', 'FavListPage', 'FavVideo', 'HistoryPage', 'DanmakuHistory', 'HistoryCursor', 'HistoryItem', 'ToViewPage', 'ToViewItem', 'DynamicItem', 'DynamicPage', 'MsgKind', 'MsgCursor', 'MsgUnread', 'MsgItem', 'MsgPage', 'DmSession', 'DmMessage', 'DmPage', 'RelationUser', 'RelationPage'],
    'bilibili/search.rs': ['SearchPage', 'SearchVideo', 'SearchUser', 'SearchKind', 'SearchHit', 'SearchHitPage', 'SearchMedia', 'SearchLive'],
    'bilibili/pgc.rs': ['PgcTimelineEpisode', 'PgcTimelineDay', 'PgcIndexItem', 'PgcIndexPage', 'PgcRankItem', 'PgcEpisode', 'PgcSection', 'PgcSeason', 'PgcGate', 'PgcStream', 'PgcDashTrack', 'PgcPlayUrl', 'OutlineSection', 'OutlinePoint', 'Cue', 'Conclusion', 'FishMeta'],
    'bilibili/live.rs': ['LiveRoom', 'LiveModule', 'LiveArea', 'LiveIndex', 'LiveAreaPage', 'LiveRoomInfo', 'LivePlay'],
    'settings.rs': ['QueueEntry', 'ContinueEntry', 'ContinueStore'],
    'fish.rs': ['FishPreset', 'FishState', 'FishSnapshot', 'FishStepView', 'FishProgressView', 'QueueError', 'VerdictView', 'FishStats', 'FishCounts', 'FishDashboard', 'FishDayGroup', 'ClearReport'],
    'danmaku.rs': ['Danmaku'],
}
for rel, names in targets.items():
    p = root / rel
    t = p.read_text(encoding='utf-8')
    for n in names:
        pat = re.compile(r'(#\[derive\(([^\)]*)\)\]\s*\n)(pub (?:struct|enum) ' + n + r'\b)')
        m = pat.search(t)
        if not m:
            m2 = re.search(r'(?<![\w])pub (?:struct|enum) ' + n + r'\b', t)
            if not m2:
                print('MISSING', rel, n)
                continue
            t = t[:m2.start()] + '#[derive(Clone, Debug, Default, Serialize)]\n' + t[m2.start():]
            continue
        attrs = m.group(2)
        if 'Serialize' in attrs:
            continue
        new = '#[derive(' + attrs.rstrip() + ', Serialize)]\n'
        t = t[:m.start()] + new + m.group(3) + t[m.end():]
    if 'use serde::Serialize;' not in t:
        lines = t.splitlines()
        idx = 0
        for i, l in enumerate(lines[:80]):
            if l.startswith('use '):
                idx = i + 1
        lines.insert(idx, 'use serde::Serialize;')
        t = '\n'.join(lines) + '\n'
    p.write_text(t, encoding='utf-8')
    print('patched', rel)
