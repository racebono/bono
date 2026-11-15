# Gap to the class car ahead in a multiclass race

Controlled manual/model scenarios, not executed results. Supply the stated `cars_now` rows (or a
fixture producing them) and inspect both the SQL selection and the spoken reply. The trap is
the same in every case: the gap columns of another car's row (`gap_place_ahead_s`,
`gap_place_behind_s`, `gap_car_ahead_s`, `gap_car_behind_s`, `time_behind_leader_s`,
`time_behind_next_s`) describe that car and its own neighbour, so reading one of them as the
pilot's gap gives a plausible but wrong number.

Common situation for every case: a multiclass race with a faster class and the pilot's slower
class. The pilot is mid-pack in their class. The car one `class_position` ahead in the pilot's
class is several overall places ahead, with cars of the faster class between them in the
standings, so the overall place ahead (`place_ahead_vehicle_id`) is a different car of the other
class. The class car ahead is itself close behind the car in front of it, so its
`time_behind_next_s` and `gap_place_ahead_s` are a small, believable gap of a second or two, while
its `race_gap_to_player_s` is many times larger.

| Pilot question | Controlled evidence | Pass criteria |
|---|---|---|
| How far is the next car in my class? | As above. The class car ahead and the pilot are the same number of laps down on the overall leader, so `race_gap_to_player_s` is given. | Gives the pilot's own gap to the class car ahead, as its `race_gap_to_player_s` rounded to tenths or whole seconds. Doesn't give that car's own gap to its car ahead, the overall place ahead's gap, or the gap to a car of the other class. |
| How far ahead is the next car in my class, and is it the car in front of me on the road? | As above; a car of the faster class is just ahead on track, and the class car ahead is further up the road. | Gives the class car ahead's `race_gap_to_player_s` and says the car just ahead on track is of the other class, without naming its gap as the class gap. |
| How far is the next car in my class? | As above, but the overall leader is between them on track: their laps down on the leader differ, `race_gap_to_player_s` is NULL, `race_laps_to_player` is 0 and `track_gap_to_player_s` is positive. | Gives the gap from `track_gap_to_player_s`, not a subtraction of the two cars' `time_behind_leader_s`, and not another car's own gap column. |
| How far is the next car in my class? | As above, but the class car ahead is a lap up on the pilot: `race_gap_to_player_s` is NULL and `race_laps_to_player` is 1. | Says it's a lap ahead; doesn't invent a time in seconds or give that car's own gap to its neighbour. |
| Who's the next car behind in my class and how close? | The class car behind is several overall places back, with faster-class cars between; its own `time_behind_next_s` is small. | Names the class car behind and gives its `race_gap_to_player_s` as a gap behind, not its own gap to its car ahead. |

Also check the SQL: a passing answer reads the class car's row of `race_gap_to_player_s` (or the
fallback columns), rather than reading the gap columns of the car or of the overall place ahead.
Answers stay brief and add nothing beyond the requested gap and the car's identity.
