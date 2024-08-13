# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.
from collections.abc import Sequence

from materialize.output_consistency.common import probability
from materialize.output_consistency.common.configuration import (
    ConsistencyTestConfiguration,
)
from materialize.output_consistency.data_value.data_column import DataColumn
from materialize.output_consistency.execution.value_storage_layout import (
    ValueStorageLayout,
)
from materialize.output_consistency.expression.expression import Expression
from materialize.output_consistency.expression.expression_with_args import (
    ExpressionWithArgs,
)
from materialize.output_consistency.generators.expression_generator import (
    ExpressionGenerator,
)
from materialize.output_consistency.ignore_filter.inconsistency_ignore_filter import (
    GenericInconsistencyIgnoreFilter,
)
from materialize.output_consistency.ignore_filter.internal_output_inconsistency_ignore_filter import (
    YesIgnore,
)
from materialize.output_consistency.input_data.constants.constant_expressions import (
    TRUE_EXPRESSION,
)
from materialize.output_consistency.input_data.operations.boolean_operations_provider import (
    NOT_OPERATION,
)
from materialize.output_consistency.input_data.operations.generic_operations_provider import (
    IS_NULL_OPERATION,
)
from materialize.output_consistency.input_data.test_input_data import (
    ConsistencyTestInputData,
)
from materialize.output_consistency.query.additional_data_source import (
    AdditionalDataSource,
)
from materialize.output_consistency.query.data_source import (
    DataSource,
)
from materialize.output_consistency.query.join import JoinTarget
from materialize.output_consistency.query.query_template import QueryTemplate
from materialize.output_consistency.selection.randomized_picker import RandomizedPicker
from materialize.output_consistency.selection.selection import (
    ALL_ROWS_SELECTION,
    DataRowSelection,
)
from materialize.output_consistency.status.consistency_test_logger import (
    ConsistencyTestLogger,
)
from materialize.output_consistency.status.test_summary import ConsistencyTestSummary


class QueryGenerator:
    """Generates query templates based on expressions"""

    def __init__(
        self,
        config: ConsistencyTestConfiguration,
        randomized_picker: RandomizedPicker,
        input_data: ConsistencyTestInputData,
        expression_generator: ExpressionGenerator,
        ignore_filter: GenericInconsistencyIgnoreFilter,
    ):
        self.config = config
        self.randomized_picker = randomized_picker
        self.input_data = input_data
        self.vertical_storage_row_count = input_data.types_input.max_value_count
        self.expression_generator = expression_generator
        self.ignore_filter = ignore_filter

        self.count_pending_expressions = 0
        # ONE query PER expression using the storage layout specified in the expression, expressions presumably fail
        self.any_layout_presumably_failing_expressions: list[Expression] = []
        # ONE query FOR ALL expressions accessing the horizontal storage layout; expressions presumably succeed and do
        # not contain aggregations
        self.horizontal_layout_normal_expressions: list[Expression] = []
        # ONE query FOR ALL expressions accessing the horizontal storage layout and applying aggregations; expressions
        # presumably succeed
        self.horizontal_layout_aggregate_expressions: list[Expression] = []
        # ONE query FOR ALL expressions accessing the vertical storage layout; expressions presumably succeed and do not
        # contain aggregations
        self.vertical_layout_normal_expressions: list[Expression] = []
        # ONE query FOR ALL expressions accessing the vertical storage layout and applying aggregations; expressions
        # presumably succeed
        self.vertical_layout_aggregate_expressions: list[Expression] = []

    def push_expression(self, expression: Expression) -> None:
        if expression.is_expect_error:
            self.any_layout_presumably_failing_expressions.append(expression)
            return

        if expression.storage_layout == ValueStorageLayout.ANY:
            # does not matter, could be taken by all
            self.vertical_layout_normal_expressions.append(expression)
        elif expression.storage_layout == ValueStorageLayout.HORIZONTAL:
            if expression.is_aggregate:
                self.horizontal_layout_aggregate_expressions.append(expression)
            else:
                self.horizontal_layout_normal_expressions.append(expression)
        elif expression.storage_layout == ValueStorageLayout.VERTICAL:
            if expression.is_aggregate:
                self.vertical_layout_aggregate_expressions.append(expression)
            else:
                self.vertical_layout_normal_expressions.append(expression)
        else:
            raise RuntimeError(f"Unknown storage layout: {expression.storage_layout}")

        self.count_pending_expressions += 1

    def shall_consume_queries(self) -> bool:
        return self.count_pending_expressions > self.config.max_pending_expressions

    def consume_queries(
        self,
        test_summary: ConsistencyTestSummary,
    ) -> list[QueryTemplate]:
        queries = []
        queries.extend(
            self._create_multi_column_queries(
                test_summary,
                self.horizontal_layout_normal_expressions,
                False,
                ValueStorageLayout.HORIZONTAL,
                False,
            )
        )
        queries.extend(
            self._create_multi_column_queries(
                test_summary,
                self.horizontal_layout_aggregate_expressions,
                False,
                ValueStorageLayout.HORIZONTAL,
                True,
            )
        )
        queries.extend(
            self._create_multi_column_queries(
                test_summary,
                self.vertical_layout_normal_expressions,
                False,
                ValueStorageLayout.VERTICAL,
                False,
            )
        )
        queries.extend(
            self._create_multi_column_queries(
                test_summary,
                self.vertical_layout_aggregate_expressions,
                False,
                ValueStorageLayout.VERTICAL,
                True,
            )
        )
        queries.extend(
            self._create_single_column_queries(
                test_summary, self.any_layout_presumably_failing_expressions
            )
        )

        self.reset_state()

        return queries

    def add_random_where_condition_to_query(self, query: QueryTemplate) -> None:
        if not self.randomized_picker.random_boolean(
            probability.GENERATE_WHERE_EXPRESSION
        ):
            return

        where_expression = self.expression_generator.generate_boolean_expression(
            False, query.storage_layout
        )

        if where_expression is None:
            return

        ignore_verdict = self.ignore_filter.shall_ignore_expression(
            where_expression, query.row_selection
        )

        if not ignore_verdict.ignore:
            query.where_expression = where_expression
            self._assign_random_sources(
                query.get_all_data_sources(), [query.where_expression]
            )

    def _create_multi_column_queries(
        self,
        test_summary: ConsistencyTestSummary,
        expressions: list[Expression],
        expect_error: bool,
        storage_layout: ValueStorageLayout,
        contains_aggregations: bool,
    ) -> list[QueryTemplate]:
        """Creates queries not exceeding the maximum column count"""
        if len(expressions) == 0:
            return []

        queries = []
        for offset_index in range(0, len(expressions), self.config.max_cols_per_query):
            expression_chunk = expressions[
                offset_index : offset_index + self.config.max_cols_per_query
            ]

            row_selection = self._select_rows(storage_layout)

            expression_chunk = self._remove_known_inconsistencies(
                test_summary, expression_chunk, row_selection
            )

            if len(expression_chunk) == 0:
                continue

            data_source, additional_data_sources = self._select_sources(storage_layout)
            all_expressions = expression_chunk
            self._assign_random_sources(
                [data_source] + additional_data_sources,
                all_expressions,
                contains_aggregations,
            )
            data_source, additional_data_sources = self.minimize_sources(
                data_source, additional_data_sources, all_expressions
            )

            query = QueryTemplate(
                expect_error,
                expression_chunk,
                None,
                storage_layout,
                data_source,
                contains_aggregations,
                row_selection,
                offset=self._generate_offset(storage_layout, contains_aggregations),
                limit=self._generate_limit(storage_layout, contains_aggregations),
                additional_data_sources=additional_data_sources,
            )

            queries.append(query)

        return queries

    def _create_single_column_queries(
        self, test_summary: ConsistencyTestSummary, expressions: list[Expression]
    ) -> list[QueryTemplate]:
        """Creates one query per expression"""

        queries = []
        for expression in expressions:
            storage_layout = expression.storage_layout

            if storage_layout == ValueStorageLayout.ANY:
                storage_layout = ValueStorageLayout.VERTICAL

            row_selection = self._select_rows(storage_layout)

            ignore_verdict = self.ignore_filter.shall_ignore_expression(
                expression, row_selection
            )
            if isinstance(ignore_verdict, YesIgnore):
                test_summary.count_ignored_select_expressions = (
                    test_summary.count_ignored_select_expressions + 1
                )
                self._log_skipped_expression(
                    test_summary, expression, ignore_verdict.reason
                )
                continue

            contains_aggregation = expression.is_aggregate

            data_source, additional_data_sources = self._select_sources(storage_layout)
            all_expressions = [expression]
            self._assign_random_sources(
                [data_source] + additional_data_sources, all_expressions
            )
            data_source, additional_data_sources = self.minimize_sources(
                data_source, additional_data_sources, all_expressions
            )

            queries.append(
                QueryTemplate(
                    expression.is_expect_error,
                    [expression],
                    None,
                    storage_layout,
                    data_source,
                    contains_aggregation,
                    row_selection,
                    offset=self._generate_offset(storage_layout, contains_aggregation),
                    limit=self._generate_limit(storage_layout, contains_aggregation),
                    additional_data_sources=additional_data_sources,
                )
            )

        return queries

    def _select_rows(self, storage_layout: ValueStorageLayout) -> DataRowSelection:
        if storage_layout == ValueStorageLayout.ANY:
            raise RuntimeError("Unresolved storage layout")
        elif storage_layout == ValueStorageLayout.HORIZONTAL:
            return ALL_ROWS_SELECTION
        elif storage_layout == ValueStorageLayout.VERTICAL:
            if self.randomized_picker.random_boolean(
                probability.RESTRICT_VERTICAL_LAYOUT_TO_2_OR_3_ROWS
            ):
                # With some probability, try to pick two or three rows
                max_number_of_rows_to_select = self.randomized_picker.random_number(
                    2, 3
                )
            else:
                # With some probability, pick an arbitrary number of rows
                max_number_of_rows_to_select = self.randomized_picker.random_number(
                    0, self.vertical_storage_row_count
                )

            row_indices = self.randomized_picker.random_row_indices(
                self.vertical_storage_row_count, max_number_of_rows_to_select
            )
            return DataRowSelection(row_indices)
        else:
            raise RuntimeError(f"Unsupported storage layout: {storage_layout}")

    def _assign_source(
        self, data_source: DataSource, expression: Expression, force: bool = False
    ) -> None:
        self._assign_random_sources([data_source], [expression], force=force)

    def _assign_random_sources(
        self,
        all_data_sources: Sequence[DataSource],
        expressions: list[Expression],
        force: bool = False,
    ) -> None:
        assert len(all_data_sources) > 0, "No data sources provided"

        for expression in expressions:
            for leaf_expression in expression.collect_leaves():
                if isinstance(leaf_expression, DataColumn):
                    random_source = self.randomized_picker.random_data_source(
                        list(all_data_sources)
                    )
                    leaf_expression.assign_data_source(random_source, force=force)

    def _select_sources(
        self,
        storage_layout: ValueStorageLayout,
    ) -> tuple[DataSource, list[AdditionalDataSource]]:
        if storage_layout == ValueStorageLayout.HORIZONTAL:
            return DataSource(table_index=None), []

        return self._random_source_tables(storage_layout)

    def minimize_sources(
        self,
        data_source: DataSource,
        additional_data_sources: list[AdditionalDataSource],
        all_expressions: list[Expression],
    ) -> tuple[DataSource, list[AdditionalDataSource]]:
        all_used_data_sources = set()

        for expression in all_expressions:
            all_used_data_sources.update(expression.collect_data_sources())

        additional_data_sources = [
            source
            for source in additional_data_sources
            if source in all_used_data_sources
        ]

        if data_source not in all_used_data_sources:
            assert len(additional_data_sources) > 0, "No data sources used"

            return (
                DataSource(
                    additional_data_sources[0].table_index,
                    additional_data_sources[0].custom_db_object_name,
                ),
                additional_data_sources[1:],
            )

        return data_source, additional_data_sources

    def _random_source_tables(
        self, storage_layout: ValueStorageLayout
    ) -> tuple[DataSource, list[AdditionalDataSource]]:
        main_source = DataSource(table_index=0)

        if self.randomized_picker.random_boolean(0.4):
            return main_source, []

        additional_sources = []
        for i in range(1, self.config.vertical_join_tables):
            if self.randomized_picker.random_boolean(0.3):
                additional_source = AdditionalDataSource(
                    table_index=i,
                    join_operator=self.randomized_picker.random_join_operator(),
                    join_constraint=TRUE_EXPRESSION,
                )
                join_constraint = self._generate_join_constraint(
                    storage_layout,
                    main_source,
                    additional_source,
                )

                if not self.ignore_filter.shall_ignore_expression(
                    join_constraint, ALL_ROWS_SELECTION
                ):
                    self._validate_join_constraint(join_constraint)
                    additional_source.join_constraint = join_constraint

                additional_sources.append(additional_source)

        return main_source, additional_sources

    def _validate_join_constraint(self, join_constraint: Expression) -> None:
        # this will fail if no data source was assigned to a leaf
        join_constraint.collect_data_sources()

    def _remove_known_inconsistencies(
        self,
        test_summary: ConsistencyTestSummary,
        expressions: list[Expression],
        row_selection: DataRowSelection,
    ) -> list[Expression]:
        indices_to_remove: list[int] = []

        for index, expression in enumerate(expressions):
            ignore_verdict = self.ignore_filter.shall_ignore_expression(
                expression, row_selection
            )
            if isinstance(ignore_verdict, YesIgnore):
                test_summary.count_ignored_select_expressions = (
                    test_summary.count_ignored_select_expressions + 1
                )
                self._log_skipped_expression(
                    test_summary, expression, ignore_verdict.reason
                )
                indices_to_remove.append(index)

        for index_to_remove in sorted(indices_to_remove, reverse=True):
            del expressions[index_to_remove]

        return expressions

    def _generate_offset(
        self, storage_layout: ValueStorageLayout, contains_aggregations: bool
    ) -> int | None:
        return self._generate_offset_or_limit(storage_layout, contains_aggregations)

    def _generate_limit(
        self, storage_layout: ValueStorageLayout, contains_aggregations: bool
    ) -> int | None:
        return self._generate_offset_or_limit(storage_layout, contains_aggregations)

    def _generate_offset_or_limit(
        self, storage_layout: ValueStorageLayout, contains_aggregations: bool
    ) -> int | None:
        if storage_layout != ValueStorageLayout.VERTICAL:
            return None

        likelihood_of_offset_or_limit = 0.025 if contains_aggregations else 0.25

        if not self.randomized_picker.random_boolean(likelihood_of_offset_or_limit):
            # do not apply it
            return None

        max_value = self.vertical_storage_row_count + 1

        if self.randomized_picker.random_boolean(0.7):
            # prefer lower numbers since queries may already contain where conditions or apply aggregations
            # (or contain offsets when generating a limit)
            max_value = int(max_value / 3)

        value = self.randomized_picker.random_number(0, max_value)

        if value == 0 and self.randomized_picker.random_boolean(0.95):
            # drop most 0 values for readability (but keep a few)
            value = None

        return value

    def _generate_join_constraint(
        self,
        storage_layout: ValueStorageLayout,
        data_source: DataSource,
        joined_source: DataSource,
    ) -> Expression:
        assert (
            storage_layout == ValueStorageLayout.VERTICAL
        ), f"Joins not supported for {storage_layout}"
        join_target = self.randomized_picker.random_join_target()

        if join_target in {
            JoinTarget.SAME_DATA_TYPE,
            JoinTarget.SAME_DATA_TYPE_CATEGORY,
            JoinTarget.ANY_COLUMN,
        }:
            random_type_with_values_1 = self.randomized_picker.random_type_with_values(
                self.input_data.types_input.all_data_types_with_values
            )

            if join_target == JoinTarget.SAME_DATA_TYPE:
                random_types_with_values_2 = [random_type_with_values_1]
            elif join_target == JoinTarget.SAME_DATA_TYPE_CATEGORY:
                random_types_with_values_2 = [
                    type_with_values
                    for type_with_values in self.input_data.types_input.all_data_types_with_values
                    if type_with_values.data_type.category
                    == random_type_with_values_1.data_type.category
                ]
            elif join_target == JoinTarget.ANY_COLUMN:
                random_types_with_values_2 = [
                    self.randomized_picker.random_type_with_values(
                        self.input_data.types_input.all_data_types_with_values
                    )
                ]
            else:
                raise RuntimeError(f"Unexpected join target: {join_target}")

            expression1 = self.expression_generator.generate_leaf_expression(
                storage_layout, [random_type_with_values_1]
            )
            expression2 = self.expression_generator.generate_leaf_expression(
                storage_layout, random_types_with_values_2
            )
            self._assign_source(data_source, expression1)
            self._assign_source(joined_source, expression2)
            return self.expression_generator.generate_equals_expression(
                expression1, expression2
            )
        elif join_target == JoinTarget.RANDOM_COLUMN_IS_NOT_NULL:
            random_type_with_values = self.randomized_picker.random_type_with_values(
                self.input_data.types_input.all_data_types_with_values
            )
            leaf_expression = self.expression_generator.generate_leaf_expression(
                storage_layout, [random_type_with_values]
            )
            self._assign_source(joined_source, leaf_expression)
            is_null_expression = ExpressionWithArgs(
                operation=IS_NULL_OPERATION,
                args=[leaf_expression],
                is_aggregate=leaf_expression.is_aggregate,
            )
            is_not_null_expression = ExpressionWithArgs(
                operation=NOT_OPERATION,
                args=[is_null_expression],
                is_aggregate=is_null_expression.is_aggregate,
            )
            return is_not_null_expression
        elif join_target == JoinTarget.BOOLEAN_EXPRESSION:
            expression = self.expression_generator.generate_boolean_expression(
                # aggregations in where conditions are not allowed
                use_aggregation=False,
                storage_layout=storage_layout,
            )
            if expression is None:
                expression = TRUE_EXPRESSION
            else:
                self._assign_source(joined_source, expression)
            return expression
        else:
            raise RuntimeError(f"Unexpected join target: {join_target}")

    def _log_skipped_expression(
        self,
        logger: ConsistencyTestLogger,
        expression: Expression,
        reason: str | None,
    ) -> None:
        if self.config.verbose_output:
            reason_desc = f" ({reason})" if reason else ""
            logger.add_global_warning(
                f"Skipping expression with known inconsistency{reason_desc}: {expression}"
            )

    def reset_state(self) -> None:
        self.count_pending_expressions = 0
        self.any_layout_presumably_failing_expressions = []
        self.horizontal_layout_normal_expressions = []
        self.horizontal_layout_aggregate_expressions = []
        self.vertical_layout_normal_expressions = []
        self.vertical_layout_aggregate_expressions = []
